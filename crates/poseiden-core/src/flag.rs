use serde::{Deserialize, Serialize};

/// How serious a hygiene violation is. Purely advisory - a flag never blocks
/// anything, but the CLI's `lint` command maps `Error` to a non-zero exit so CI
/// can gate on it, and the UI colours by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warn,
    Error,
}

/// Machine-readable code for a hygiene violation. The human-readable detail
/// (which tag, how many days) lives in [`Flag::message`]; this stays a stable
/// enum so the UI can filter/group and the CLI can count by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagCode {
    /// Item has no tags at all.
    Untagged,
    /// Item is missing a tag the ruleset requires.
    MissingRequiredTag,
    /// Item carries a tag not on the allowed list (or on the deny list).
    DisallowedTag,
    /// Item has sat in its current state longer than the ruleset permits.
    Stale,
    /// Item is in a resolved/terminal state yet still carries a tag that implies
    /// outstanding work (e.g. "to refine" on a Closed story) - a contradiction the
    /// content-only AI tagger can't catch. See `RuleSet::stale_when_resolved_tags`.
    StaleStateTag,
    /// Item has too little descriptive body to work with (see
    /// `poseiden_rules::is_underspecified`) - the most upstream hygiene problem, since
    /// you can't meaningfully tag, estimate or review it until it's fleshed out. Only
    /// raised when `refine_tag` is configured and the item is open (not resolved).
    Underspecified,
    /// Item shares its (normalised) title with another item in scope - a likely
    /// copy-paste / raised-twice duplicate to merge. Opt-in (`flag_duplicate_titles`),
    /// since some backlogs have legitimately recurring titles.
    Duplicate,
    /// Item's title is a placeholder / junk (`test`, `asdf`, `Untitled`, too short) -
    /// it tells you nothing about the work. Driven by the configured `bad_title_terms`.
    BadTitle,
    /// An on-demand AI healthcheck judged this item's data quality as poor (a vague
    /// title, contradictory / missing description, malformed data). Advisory and
    /// AI-sourced - the specific concern is in [`Flag::message`]. Only present after a
    /// person runs the healthcheck; stored until re-run (see the healthcheck audit).
    AiAudit,
}

/// A single hygiene violation against one work item. Produced by the rules
/// engine; consumed by the dashboard, the tickets view, and the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub work_item_id: i64,
    pub team: String,
    pub code: FlagCode,
    pub severity: Severity,
    /// Human-readable explanation, e.g. `missing required tag "team:platform"`
    /// or `stale: 9 days in "In Progress" (limit 5)`.
    pub message: String,
    /// The specific tag involved, when the flag is about one tag. `None` for
    /// `Untagged` / `Stale`. Lets the UI offer "add this tag" affordances
    /// later without re-parsing the message.
    pub tag: Option<String>,
}

/// A hygiene flag for a non-work-item entity (a pipeline or pull request). Work
/// items use the richer [`Flag`]; these domains only need a code + severity +
/// message, carried inline on the entity's read DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityFlag {
    /// Machine slug, e.g. `"failing"`, `"never-run"`, `"stale-open"`.
    pub code: String,
    pub severity: Severity,
    pub message: String,
}

/// A suggested tag for a work item, with the keyword(s) that triggered it. A
/// runtime, read-only hint (computed on read, never stored) - applying it is a
/// user action that writes the tag through the normal edit path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSuggestion {
    /// The tag to add (the exact string used everywhere else).
    pub tag: String,
    /// The keyword(s) matched in the item's title - the "why".
    pub reasons: Vec<String>,
    /// When this is a REWRITE (a tag alias, e.g. legacy `SSA` -> canonical
    /// `area:ssa`), the existing tag it should replace. Applying the suggestion then
    /// removes `replaces` and adds `tag` in one edit. `None` = a plain add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_code_serializes_snake_case() {
        // The stable wire slugs the UI filters on and the CLI counts by.
        let cases = [
            (FlagCode::Untagged, "untagged"),
            (FlagCode::MissingRequiredTag, "missing_required_tag"),
            (FlagCode::DisallowedTag, "disallowed_tag"),
            (FlagCode::Stale, "stale"),
            (FlagCode::StaleStateTag, "stale_state_tag"),
            (FlagCode::Underspecified, "underspecified"),
            (FlagCode::Duplicate, "duplicate"),
            (FlagCode::BadTitle, "bad_title"),
            (FlagCode::AiAudit, "ai_audit"),
        ];
        for (code, slug) in cases {
            assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(slug));
            // And back the other way.
            let back: FlagCode = serde_json::from_value(serde_json::json!(slug)).unwrap();
            assert_eq!(back, code);
        }
    }

    #[test]
    fn severity_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(Severity::Warn).unwrap(),
            serde_json::json!("warn")
        );
        assert_eq!(
            serde_json::to_value(Severity::Error).unwrap(),
            serde_json::json!("error")
        );
        let w: Severity = serde_json::from_value(serde_json::json!("warn")).unwrap();
        assert_eq!(w, Severity::Warn);
    }

    #[test]
    fn tag_suggestion_skips_replaces_when_none() {
        // A plain add: `replaces` must be OMITTED (not null) so consumers can
        // treat presence as "this is a rewrite".
        let s = TagSuggestion {
            tag: "type:bug".into(),
            reasons: vec!["crash".into()],
            replaces: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("replaces").is_none(), "replaces omitted when None");
        assert_eq!(v.get("tag").unwrap(), &serde_json::json!("type:bug"));
    }

    #[test]
    fn tag_suggestion_includes_replaces_when_some() {
        // A rewrite carries the legacy tag it supersedes.
        let s = TagSuggestion {
            tag: "area:ssa".into(),
            reasons: vec!["alias".into()],
            replaces: Some("SSA".into()),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v.get("replaces").unwrap(), &serde_json::json!("SSA"));
        // Round-trips back to the same value.
        let back: TagSuggestion = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn tag_suggestion_replaces_defaults_to_none() {
        // Absent `replaces` on the wire deserialises to None (the default).
        let s: TagSuggestion =
            serde_json::from_str(r#"{ "tag": "type:bug", "reasons": [] }"#).unwrap();
        assert!(s.replaces.is_none());
    }

    #[test]
    fn flag_roundtrips() {
        let f = Flag {
            work_item_id: 42,
            team: "Platform".into(),
            code: FlagCode::MissingRequiredTag,
            severity: Severity::Error,
            message: r#"missing required tag "team:platform""#.into(),
            tag: Some("team:platform".into()),
        };
        let v = serde_json::to_value(&f).unwrap();
        let back: Flag = serde_json::from_value(v).unwrap();
        assert_eq!(back, f);
    }
}
