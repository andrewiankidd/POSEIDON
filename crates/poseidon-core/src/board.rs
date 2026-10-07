//! Kanban board definitions - the columns a tracker's board shows.
//!
//! A board's columns are NOT the same thing as work-item states: Azure DevOps lets a
//! team add custom columns (for example "To prioritize" and "This week") that several map
//! onto one state, so grouping by state alone cannot reproduce the tracker's own board.
//! Providers that have columns discover them into these provider-agnostic shapes and stamp
//! each work item with the column it sits in (see `WorkItem::board_column`).

use serde::{Deserialize, Serialize};

/// Where a column sits in the flow, which decides how a view treats it (a board normally
/// shows only recently-finished work in its outgoing column, otherwise it'd be endless).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    /// The intake column (Azure DevOps "incoming", e.g. New).
    Incoming,
    /// A normal working column.
    #[default]
    InProgress,
    /// The finished column (Azure DevOps "outgoing", e.g. Closed).
    Outgoing,
}

/// One column of a [`Board`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumn {
    pub name: String,
    /// Work-in-progress limit; `None` (or 0 from the tracker) means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wip_limit: Option<u32>,
    #[serde(default)]
    pub kind: ColumnKind,
    /// The column is split into Doing / Done halves.
    #[serde(default)]
    pub split: bool,
}

/// A tracker board at one backlog level (Azure DevOps: Stories, Features, Epics).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// Display name, e.g. `Stories`.
    pub name: String,
    /// Columns in the tracker's left-to-right order.
    pub columns: Vec<BoardColumn>,
    /// Swimlane names, if the board uses them (the default lane is not listed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<String>,
    /// The work-item types that live on this board (an item belongs to the board whose
    /// list contains its type). Compared case-insensitively.
    #[serde(default)]
    pub work_item_types: Vec<String>,
}

impl Board {
    /// Whether work items of `work_item_type` appear on this board.
    pub fn covers(&self, work_item_type: &str) -> bool {
        self.work_item_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(work_item_type))
    }
}

/// One team's boards, as served to the UI (a scope of "all teams" returns several).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamBoards {
    pub team: String,
    pub boards: Vec<Board>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_covers_its_types_case_insensitively() {
        let b = Board {
            name: "Stories".into(),
            columns: vec![],
            lanes: vec![],
            work_item_types: vec!["User Story".into(), "Bug".into()],
        };
        assert!(b.covers("user story"));
        assert!(b.covers("Bug"));
        assert!(!b.covers("Epic"));
    }

    #[test]
    fn columns_round_trip_with_defaults() {
        let c: BoardColumn = serde_json::from_str(r#"{"name":"New"}"#).unwrap();
        assert_eq!(c.kind, ColumnKind::InProgress);
        assert_eq!(c.wip_limit, None);
        assert!(!c.split);
        let j = serde_json::to_value(BoardColumn {
            name: "Closed".into(),
            wip_limit: Some(25),
            kind: ColumnKind::Outgoing,
            split: false,
        })
        .unwrap();
        assert_eq!(j["kind"], "outgoing");
        assert_eq!(j["wip_limit"], 25);
    }
}
