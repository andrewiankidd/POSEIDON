//! Capacity: how much demand a team faces and how much it gets through, without sprints or
//! story points. The shapes here are provider-agnostic; `poseidon-reports::capacity` computes a
//! [`CapacityReport`] from the stored work items, pull requests and commit activity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One commit, reduced to what effort analysis needs: who, where, and when. Collected from the
/// tracker's repositories on a slow cadence (it is only used for the hour-of-day / streak
/// picture, never for the content of the work).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitActivity {
    /// Provider commit id (a SHA).
    pub id: String,
    /// The team this was collected under (the `[[team]]` name).
    pub team: String,
    /// Repository name.
    pub repo: String,
    /// Author display name.
    pub author: String,
    /// Author email / sign-in, when the provider gives one - what roster matching keys on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// When it was authored (UTC).
    pub authored_at: DateTime<Utc>,
}

/// What kind of demand a work item represents, as judged by the AI model for items that carry no
/// `source:` tag. Advisory: kept locally, never written back to the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandClass {
    /// An escalation, support request, incident or ad hoc ask from outside the team.
    Reactive,
    /// Committed roadmap / project delivery.
    Planned,
    /// Upgrades, tech debt and housekeeping the team chose to do.
    Maintenance,
    /// A running list / catch-all / ongoing container - not a deliverable.
    Standing,
}

impl DemandClass {
    /// Stable lowercase slug used for storage + the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            DemandClass::Reactive => "reactive",
            DemandClass::Planned => "planned",
            DemandClass::Maintenance => "maintenance",
            DemandClass::Standing => "standing",
        }
    }

    /// Parse a stored / model-supplied slug (case-insensitive); `None` when it isn't one.
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reactive" => Some(DemandClass::Reactive),
            "planned" => Some(DemandClass::Planned),
            "maintenance" => Some(DemandClass::Maintenance),
            "standing" => Some(DemandClass::Standing),
            _ => None,
        }
    }
}

/// A stored AI verdict for one work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandVerdictRecord {
    pub work_item_id: i64,
    pub team: String,
    pub class: DemandClass,
    /// The model's guess at the originating source (`support`, `request`, `incident`, `sre`,
    /// `external`, `roadmap`, `fpp`), when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Hash of the title the verdict was made from, so a retitled item is classified again.
    pub title_hash: String,
}

// ── The report ──────────────────────────────────────────────────────────────

/// One calendar week (Monday start) of flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeekFlow {
    /// The Monday, `YYYY-MM-DD`.
    pub week_start: String,
    pub arrived: u32,
    pub closed: u32,
    /// A clean-up week: far more closed than is typical, so it is not read as capacity.
    pub bulk: bool,
}

/// Demand in against work out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowSummary {
    pub arrived: u32,
    pub closed: u32,
    /// Arrivals per completion (`arrived / closed`); above 1 the backlog is growing.
    pub ratio: Option<f64>,
    /// Net items added to the backlog per week.
    pub backlog_growth_per_week: f64,
    /// A typical week's completions, with bulk clean-up weeks left out.
    pub typical_weekly_throughput: f64,
    pub bulk_weeks: u32,
    /// Completions in bulk weeks beyond a typical week.
    pub bulk_closed: u32,
    /// Open, non-standing items waiting (flow types only).
    pub open_backlog: u32,
    /// Weeks to clear the open backlog at the typical rate with nothing new arriving.
    pub weeks_to_clear: Option<f64>,
}

/// How long finished items took, creation to done.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CycleTime {
    pub n: u32,
    pub median_days: Option<f64>,
    pub p85_days: Option<f64>,
    pub p95_days: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonWip {
    pub name: String,
    pub in_progress: u32,
    pub open_assigned: u32,
}

/// What is in flight right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WipSummary {
    pub in_progress: u32,
    /// In progress but untouched for 30+ / 90+ days.
    pub stalled_30: u32,
    pub stalled_90: u32,
    pub unassigned_open: u32,
    pub per_person: Vec<PersonWip>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCount {
    pub source: String,
    pub count: u32,
    /// How many of `count` came from an AI verdict rather than a tag.
    pub inferred: u32,
}

/// Where the arrivals in the window came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DemandMix {
    pub total: u32,
    pub reactive: u32,
    pub planned: u32,
    pub maintenance: u32,
    /// No source tag and no AI verdict.
    pub unknown: u32,
    /// Classified from `source:` tags / by an AI verdict.
    pub tagged: u32,
    pub inferred: u32,
    /// `reactive / (total - unknown)`; `None` when nothing is classified.
    pub reactive_share: Option<f64>,
    pub by_source: Vec<SourceCount>,
}

/// A "running list" style item, left out of throughput.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandingItem {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub age_days: i64,
    /// `title` (matched a running-list phrase) or `ai`.
    pub reason: String,
}

/// How concentrated the merged work on one repository is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoConcentration {
    pub repo: String,
    pub prs: u32,
    pub authors: u32,
    pub top_author: String,
    pub top_share: f64,
    /// One person wrote nearly all of a repo with real volume - the "who else can do this" risk.
    pub single_point: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConcentrationSummary {
    pub merged_prs: u32,
    /// The fewest people who account for half of all merged PRs.
    pub people_for_half: u32,
    pub top_author: Option<String>,
    pub top_share: Option<f64>,
    pub repos: Vec<RepoConcentration>,
}

/// One person's working-hours picture, from commit timestamps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonEffort {
    pub name: String,
    /// The commit author email, when known - what a profile is keyed on.
    #[serde(default)]
    pub unique_name: Option<String>,
    /// IANA timezone their hours are read in; `None` = the viewer's clock (not configured).
    #[serde(default)]
    pub timezone: Option<String>,
    pub commits: u32,
    pub active_days: u32,
    /// Weekdays in the window they were expected to work: all weekdays so far minus leave.
    #[serde(default)]
    pub available_days: u32,
    /// Weekdays in the window covered by recorded leave.
    #[serde(default)]
    pub leave_days: u32,
    /// Days within recorded leave on which they still committed.
    #[serde(default)]
    pub worked_on_leave_days: u32,
    /// Share of commits outside 09:00-18:00 Monday-Friday.
    pub after_hours_share: f64,
    pub weekend_days: u32,
    /// Distinct days with a commit at/after 22:00 or before 05:00.
    pub late_nights: u32,
    /// Longest run of consecutive calendar days with a commit.
    pub longest_streak: u32,
    /// Longest first-to-last-commit span within one local day.
    pub longest_span_hours: f64,
    /// Commits per local hour of day, 0-23.
    pub hours: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffortSummary {
    /// False when no commit activity has been collected yet.
    pub available: bool,
    pub commits: u32,
    pub after_hours_share: Option<f64>,
    /// Commits per local hour of day, 0-23.
    pub hours: Vec<u32>,
    /// `[weekday 0=Mon..6=Sun][hour]` commit counts.
    pub heat: Vec<Vec<u32>>,
    pub people: Vec<PersonEffort>,
}

/// Everything the Capacity screen shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityReport {
    pub generated_at: DateTime<Utc>,
    pub weeks: u32,
    pub window_start: String,
    pub window_end: String,
    pub flow: FlowSummary,
    pub weekly: Vec<WeekFlow>,
    pub cycle: CycleTime,
    pub wip: WipSummary,
    pub demand: DemandMix,
    pub standing: Vec<StandingItem>,
    pub concentration: ConcentrationSummary,
    pub effort: EffortSummary,
    /// Plain-language findings, computed (no AI): what a reader should take from the numbers.
    pub facts: Vec<String>,
    /// Honest caveats about the data behind the numbers.
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_classes_round_trip_their_slugs() {
        for c in [
            DemandClass::Reactive,
            DemandClass::Planned,
            DemandClass::Maintenance,
            DemandClass::Standing,
        ] {
            assert_eq!(DemandClass::from_slug(c.as_str()), Some(c));
        }
        assert_eq!(
            DemandClass::from_slug("  Reactive "),
            Some(DemandClass::Reactive)
        );
        assert_eq!(DemandClass::from_slug("urgent"), None);
    }
}
