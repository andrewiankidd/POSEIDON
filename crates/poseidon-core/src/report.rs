use serde::{Deserialize, Serialize};

/// A tag paired with a count - the unit of the "breakdown by tag" report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: i64,
}

/// Ticket-flow report over a date range. Powers the Reports view's ticket
/// half. Counts are whole-range totals; `closed_by_tag` is the grouped
/// breakdown the chart renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketReport {
    /// Inclusive ISO-8601 date range the numbers cover.
    pub from: String,
    pub to: String,
    pub opened: i64,
    pub closed: i64,
    /// Closed-item counts grouped by tag, descending. An item with N tags
    /// contributes to N buckets; untagged closed items fall under the
    /// synthetic `"(untagged)"` bucket so the totals stay legible.
    pub closed_by_tag: Vec<TagCount>,
}

/// Pipeline-flow report over the same date range. Powers the Reports view's
/// pipeline half.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineReport {
    pub from: String,
    pub to: String,
    pub total_runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub canceled: i64,
    /// Succeeded / (succeeded + failed), 0.0-1.0. Excludes canceled + running
    /// so a burst of manual cancels doesn't tank the number. `None` when there
    /// were no completed runs to divide by.
    pub success_rate: Option<f64>,
}

/// Live pipeline-health snapshot for the dashboard + pipeline view. Not a
/// date-range report - this is "right now".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineHealth {
    pub pipeline_id: i64,
    pub name: String,
    /// Web URL to the pipeline definition in the provider's UI (open in the
    /// provider), distinct from `last_run_url` which points at a specific run.
    pub url: String,
    /// Pipeline folder / virtual-folder path in the provider (e.g. Azure DevOps
    /// build-definition folders like `\Platform\CI`), if it lives in one. `None`
    /// for a root-level pipeline.
    pub folder: Option<String>,
    pub team: String,
    /// Status of the most recent run, or `None` if the pipeline has never run.
    pub last_status: Option<crate::RunStatus>,
    /// ISO-8601 timestamp of the most recent failure, if any - the "last
    /// failure timestamp" the brief calls for.
    pub last_failure_at: Option<String>,
    /// ISO-8601 timestamp of the most recent run (any result), so the UI can show
    /// "ran 2y ago" and distinguish a stale success from a fresh one.
    pub last_run_at: Option<String>,
    /// Quick link to the latest run's logs.
    pub last_run_url: Option<String>,
    pub succeeded: i64,
    pub failed: i64,
    pub running: i64,
    /// Hygiene flags for this pipeline (failing / never-run), evaluated against
    /// the team's effective ruleset. Empty when it's healthy or no checks are on.
    #[serde(default)]
    pub flags: Vec<crate::EntityFlag>,
}

/// Everything the dashboard needs in one round-trip: hygiene flag counts +
/// pipeline health at a glance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSummary {
    pub total_work_items: i64,
    pub flagged_items: i64,
    /// Flag counts keyed by `FlagCode` serialised form (`untagged`,
    /// `missing_required_tag`, …) so the UI can render a labelled row.
    pub flags_by_code: Vec<TagCount>,
    pub pipelines: Vec<PipelineHealth>,
    /// Open (active, non-draft) pull requests in flight.
    pub open_prs: i64,
    /// Draft (active, work-in-progress) pull requests.
    pub draft_prs: i64,
    /// Active PRs carrying at least one hygiene flag (stale / no-work-item).
    #[serde(default)]
    pub flagged_prs: i64,
    /// PR flag counts by code (`stale-open`, `stale-draft`, `no-work-item`).
    #[serde(default)]
    pub pr_flags_by_code: Vec<TagCount>,
    /// Pipelines carrying at least one hygiene flag (never-run / failing).
    #[serde(default)]
    pub flagged_pipelines: i64,
    /// Pipeline flag counts by code (`never-run`, `failing`).
    #[serde(default)]
    pub pipeline_flags_by_code: Vec<TagCount>,
    /// When the last successful poll completed, ISO-8601. `None` before the
    /// first poll.
    pub last_polled_at: Option<String>,
}
