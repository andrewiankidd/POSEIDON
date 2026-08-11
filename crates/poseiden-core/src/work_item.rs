use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A normalised work item (ticket) from any provider.
///
/// Provider-specific fields are flattened into this common shape by the
/// provider crate. `tags` is always the split, trimmed list - never the
/// provider's raw joined string (some providers ship tags as a single
/// `"a; b; c"` field; that splitting happens at normalisation time so every
/// downstream consumer sees a clean `Vec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Provider-native id (Azure DevOps work-item id, Jira key hashed, etc).
    pub id: i64,
    /// Which provider this came from. Lets a multi-team instance mixing
    /// providers keep ids unambiguous.
    pub provider: String,
    /// The team (board) this item belongs to - the `name` of the `[[team]]`
    /// config entry it was polled from. The scope selector filters on this.
    pub team: String,
    pub title: String,
    /// Item type - "Bug", "User Story", "Task", "Feature", … Provider-native
    /// string; POSEIDEN doesn't impose a taxonomy.
    pub work_item_type: String,
    /// Workflow state - "New", "Active", "In Progress", "Closed", …
    pub state: String,
    /// Normalised tag list (split + trimmed). Empty = untagged.
    pub tags: Vec<String>,
    /// Display name of the assignee, or `None` if unassigned.
    pub assigned_to: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Last-modified timestamp - the anchor for staleness checks.
    pub changed_at: DateTime<Utc>,
    /// Set once the item reaches a closed/done state.
    pub closed_at: Option<DateTime<Utc>>,
    /// Iteration / sprint path, if the provider exposes one. Powers the
    /// future sprint view.
    pub iteration_path: Option<String>,
    /// Effort estimate (story points), if set. Powers the future
    /// cost/effort report.
    pub story_points: Option<f64>,
    /// Free-text body/description (HTML stripped to plain text), fetched from the
    /// provider. Fed into tag suggestion (AI + keyword) when the owner opts in.
    /// `skip_serializing` keeps it OUT of the work-items list payload (5k bodies would
    /// bloat it); the WebGPU tagger pulls bodies via the dedicated descriptions endpoint.
    #[serde(default, skip_serializing)]
    pub description: Option<String>,
    /// Web URL to the item in the provider's UI (for "open in the provider").
    pub url: String,
    /// Ids of pull requests linked to this item (from the item's provider
    /// relations). Stored so the link survives between polls.
    #[serde(default)]
    pub linked_pr_ids: Vec<i64>,
    /// The linked PRs resolved to display shape (id + status + url), filled on
    /// read by joining `linked_pr_ids` with the polled PR set. Runtime-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_prs: Vec<crate::LinkedPr>,
    /// Tags auto-suggested from the ruleset's keyword map (computed on read, not
    /// stored). Advisory - the user applies one via the normal tag edit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tag_suggestions: Vec<crate::TagSuggestion>,
}

impl WorkItem {
    /// Whether this item carries no tags at all.
    pub fn is_untagged(&self) -> bool {
        self.tags.is_empty()
    }

    /// Age of the item since its last modification, as of `now`. Used by
    /// staleness rules; a negative-clock skew is clamped to zero days.
    pub fn days_since_change(&self, now: DateTime<Utc>) -> i64 {
        (now - self.changed_at).num_days().max(0)
    }
}

/// A requested change to a work item's editable fields. Each field is
/// `Option`-al so a caller can update just one (e.g. only the state) without
/// touching the other. This is the wire type for the update endpoint / command;
/// the provider translates present fields into provider-native patch operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemUpdate {
    /// New workflow state (e.g. "Resolved"). `None` = leave unchanged.
    pub state: Option<String>,
    /// Full replacement tag list. `None` = leave unchanged; `Some(vec![])`
    /// clears all tags.
    pub tags: Option<Vec<String>>,
}

impl WorkItemUpdate {
    /// Whether this update actually changes anything.
    pub fn is_empty(&self) -> bool {
        self.state.is_none() && self.tags.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> DateTime<Utc> {
        iso.parse().expect("valid timestamp")
    }

    fn sample() -> WorkItem {
        WorkItem {
            id: 7,
            provider: "azure-dev-ops".into(),
            team: "Platform".into(),
            title: "Fix the widget".into(),
            work_item_type: "Bug".into(),
            state: "Active".into(),
            tags: vec!["type:bug".into()],
            assigned_to: Some("Ada".into()),
            created_at: at("2026-01-01T00:00:00Z"),
            changed_at: at("2026-01-05T00:00:00Z"),
            closed_at: None,
            iteration_path: None,
            story_points: Some(3.0),
            description: Some("a long body".into()),
            url: "https://example.com/7".into(),
            linked_pr_ids: vec![],
            linked_prs: vec![],
            tag_suggestions: vec![],
        }
    }

    #[test]
    fn description_never_serializes_out() {
        // `skip_serializing` keeps 5k bodies out of the list payload - even when
        // the field IS populated it must not appear on the wire.
        let wi = sample();
        assert!(wi.description.is_some());
        let v = serde_json::to_value(&wi).unwrap();
        assert!(v.get("description").is_none(), "description skipped on serialize");
    }

    #[test]
    fn description_deserializes_when_present() {
        // The dedicated descriptions endpoint feeds bodies back IN, so the field
        // must still deserialise (the skip is serialize-only).
        let mut v = serde_json::to_value(sample()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("description".into(), serde_json::json!("hydrated body"));
        let wi: WorkItem = serde_json::from_value(v).unwrap();
        assert_eq!(wi.description.as_deref(), Some("hydrated body"));
    }

    #[test]
    fn description_defaults_to_none_when_absent() {
        // Absent on the wire (the normal list-payload case) -> None.
        let wi: WorkItem = serde_json::from_value(serde_json::to_value(sample()).unwrap()).unwrap();
        assert!(wi.description.is_none());
    }

    #[test]
    fn is_untagged_reflects_tag_list() {
        let mut wi = sample();
        assert!(!wi.is_untagged());
        wi.tags.clear();
        assert!(wi.is_untagged());
    }

    #[test]
    fn days_since_change_counts_and_clamps() {
        let wi = sample(); // changed_at = 2026-01-05
        assert_eq!(wi.days_since_change(at("2026-01-14T00:00:00Z")), 9);
        // Clock skew (now before changed_at) clamps to zero, never negative.
        assert_eq!(wi.days_since_change(at("2026-01-01T00:00:00Z")), 0);
    }

    #[test]
    fn work_item_update_is_empty() {
        assert!(WorkItemUpdate::default().is_empty());
        assert!(!WorkItemUpdate { state: Some("Resolved".into()), tags: None }.is_empty());
        // Clearing all tags (Some(empty vec)) is still a change.
        assert!(!WorkItemUpdate { state: None, tags: Some(vec![]) }.is_empty());
    }
}
