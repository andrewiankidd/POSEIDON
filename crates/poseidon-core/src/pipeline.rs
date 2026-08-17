use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A CI/CD pipeline definition owned/observed for health monitoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    /// Provider-native pipeline/definition id.
    pub id: i64,
    pub provider: String,
    /// The team (board) this pipeline belongs to - the `[[team]]` config `name`.
    pub team: String,
    pub name: String,
    /// Pipeline folder / path, if the provider groups them.
    pub folder: Option<String>,
    /// Web URL to the pipeline in the provider's UI.
    pub url: String,
    /// Status of the pipeline's most recent completed run, fetched regardless of
    /// the poll's recent-runs window - so a pipeline that last ran years ago
    /// still shows its real status rather than "never run". `None` = truly never
    /// run.
    #[serde(default)]
    pub last_run_status: Option<RunStatus>,
    /// When that most-recent run finished.
    #[serde(default)]
    pub last_run_at: Option<DateTime<Utc>>,
    /// Link to that most-recent run.
    #[serde(default)]
    pub last_run_url: Option<String>,
}

/// The outcome of a single pipeline run, normalised across providers.
///
/// Some providers split this across `status` (inProgress/completed) and
/// `result` (succeeded/failed/canceled); we fold both into one enum because
/// downstream consumers only care about the effective state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    Running,
    Canceled,
    /// Provider reported a state we don't map - surfaced honestly rather than
    /// silently bucketed as success or failure.
    Unknown,
}

impl RunStatus {
    /// Terminal states - a run that will not change. `Running` is the only
    /// non-terminal variant.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, RunStatus::Running)
    }
}

/// A single execution of a [`Pipeline`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    /// Provider-native run/build id.
    pub id: i64,
    pub pipeline_id: i64,
    pub provider: String,
    pub team: String,
    pub status: RunStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Branch the run was triggered against (e.g. `refs/heads/main`).
    pub source_branch: Option<String>,
    /// Web URL to this run's logs - the "quick link to logs" affordance.
    pub url: String,
}
