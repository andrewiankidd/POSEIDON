//! POSEIDEN's domain layer - provider-agnostic, IO-free types.
//!
//! This crate is the bottom of the dependency graph and the **single wire
//! schema** shared by every layer: the same `serde` structs cross the HTTP
//! boundary (browser → server), the Tauri IPC boundary (webview → shell), and
//! the CLI's stdout. Defining them once is what keeps the three delivery
//! shells honest with each other.
//!
//! What lives here: normalised entities (`WorkItem`, `Pipeline`, `PipelineRun`),
//! rule-evaluation outputs (`Flag`), reporting DTOs, and the config schema
//! (`PoseidenConfig` + `RuleSet`). What does NOT live here: any IO, any
//! provider-specific parsing, and any algorithm heavier than a getter. Adding a
//! runtime dependency to this crate is a smell - push it up a layer.
//!
//! ## Provider-agnostic by construction
//!
//! Azure DevOps is the first provider, but nothing here names it. A `WorkItem`
//! is just an id + title + state + tags + timestamps; whether it came from
//! Azure DevOps, Jira, or GitHub Issues is the [`ProviderKind`] tag and the
//! provider crate's problem, not the domain's. New providers normalise *into*
//! these shapes.

mod config;
mod config_bundle;
mod flag;
mod pipeline;
mod pull_request;
mod report;
mod report_spec;
mod work_item;

pub use config::{
    parse_duration, AzureDevOpsAuth, DoctorConfig, PipelineRules, PoseidenConfig, PrRules,
    ProviderKind, RuleSet, ServerConfig, StaleRule, TagAlias, TagKeywords, TeamConfig, UserConfig,
};
pub use config_bundle::{BundleMeta, ConfigBundle, CONFIG_BUNDLE_SCHEMA};
pub use flag::{EntityFlag, Flag, FlagCode, Severity, TagSuggestion};
pub use pipeline::{Pipeline, PipelineRun, RunStatus};
pub use pull_request::{LinkedPr, PrStatus, PullRequest};
pub use report::{DashboardSummary, PipelineHealth, PipelineReport, TagCount, TicketReport};
pub use report_spec::{
    Condition, DataSource, GroupBy, Metric, Op, Point, RenderKind, ReportResult, ReportSpec,
    ResultSeries, Series, TimeRange,
};
pub use work_item::{WorkItem, WorkItemUpdate};

/// The implicit single owner every stored row is scoped to in the PoC. There
/// is no auth yet (the hosted instance is gated at the mesh, e.g. Istio), so
/// all data belongs to this well-known owner. When multi-user auth lands,
/// real user identifiers replace `DEFAULT_OWNER` and existing rows migrate
/// under it - the owner column already exists, so the change is additive.
pub const DEFAULT_OWNER: &str = "default";

/// Nautical mottos, matching the name. Shown in the CLI banner and in the app
/// (under the wordmark, and appended to the window / browser-tab title). Each
/// surface picks one at random per launch. Kept here so the CLI and any Rust
/// surface share one list; the frontend mirrors it in JS.
pub const MOTDS: &[&str] = &[
    "Weather the storm",
    "Stem the tide",
    "Ride the wave",
    "Keep your head above water",
    "Stay afloat",
];

/// The running version, for the Doctor's update check + the About box.
///
/// Resolved in order: the `POSEIDEN_VERSION` runtime env var (handy for testing
/// the update path locally), the same var baked in at build time (CI stamps the
/// git tag), else `"local"`. A `"local"` version means a development build, and
/// the update check skips rather than nagging.
pub fn version() -> String {
    std::env::var("POSEIDEN_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| option_env!("POSEIDEN_VERSION").map(str::to_string))
        .unwrap_or_else(|| "local".to_string())
}

/// Whether this is a development (`"local"`) build.
pub fn is_local_build() -> bool {
    version() == "local"
}

/// The git commit this build was made from, or `"unknown"` in dev. Resolved
/// from the `POSEIDEN_COMMIT` runtime env var, then the build-time var (CI
/// stamps `github.sha`), else `"unknown"`. Used by the update check to detect a
/// moved rolling tag (e.g. `latest-main`), where the tag NAME never changes but
/// the commit does.
pub fn commit() -> String {
    std::env::var("POSEIDEN_COMMIT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| option_env!("POSEIDEN_COMMIT").map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}
