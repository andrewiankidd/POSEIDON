//! Durable AI state shared across transports: the "Improve all" field drafts and the
//! activity-log records. Both are owner-scoped in the store; these are the wire shapes.

use serde::{Deserialize, Serialize};

/// One pending "Improve all fields" draft for a work-item field, persisted server-side
/// so the ✨ badge + editor pre-fill survive a refresh (and reach another machine).
/// Cleared when the field is reviewed/applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiFieldDraft {
    /// Provider field reference, e.g. `System.Description`.
    pub reference: String,
    /// The AI-proposed markdown value awaiting review.
    pub value: String,
}

/// One AI job in the activity log - a run of tag-suggest / healthcheck / improve-all,
/// with its per-item results. Upserted by `id` as the run progresses so the queue can be
/// rebuilt after a refresh, and kept as an audit trail of what the AI proposed. `items`
/// is opaque JSON owned by the frontend (the per-item rows the activity panel renders).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiActivityRecord {
    /// Stable job id (client-generated), unique per owner.
    pub id: String,
    #[serde(default)]
    pub team: String,
    /// Display name, e.g. `Suggest tags`.
    pub name: String,
    /// Where it ran: `gpu` (browser WebGPU) or `server`.
    #[serde(default, rename = "where")]
    pub where_at: String,
    /// `running` | `done` | `failed` | `cancelled`.
    pub status: String,
    #[serde(default)]
    pub done: i64,
    #[serde(default)]
    pub total: i64,
    /// Short outcome summary shown in the completed list.
    #[serde(default)]
    pub outcome: String,
    /// Per-item results as a JSON array (frontend-defined shape).
    #[serde(default)]
    pub items: serde_json::Value,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub updated_at: String,
}
