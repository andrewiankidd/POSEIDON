use serde::{Deserialize, Serialize};

/// How an [`EditableField`] should be rendered + edited. The provider maps its
/// native field types onto this small, UI-facing set so the editor modal stays
/// provider-agnostic - it never sees an Azure DevOps reference type or a GitHub
/// body directly, only a `FieldKind`.
///
/// Rich text is always `Markdown`: Azure DevOps stores HTML, GitHub/GitLab store
/// markdown, but the provider normalises BOTH to markdown on the way out (and back
/// on the way in), so the editor only ever deals with one rich format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// Multi-line rich text, edited as markdown (Description, Repro Steps,
    /// Acceptance Criteria, a GitHub issue body …).
    Markdown,
    /// Multi-line plain text (no formatting).
    PlainText,
    /// Single-line string.
    Text,
    /// Whole number.
    Integer,
    /// Decimal number (e.g. story points, effort).
    Float,
    /// A date/time (ISO-8601 on the wire).
    DateTime,
    /// A yes/no flag.
    Boolean,
    /// One of a fixed set of `options`.
    Select,
    /// A person (assignee, etc.) - edited as a display name/email for now.
    Identity,
}

impl FieldKind {
    /// Whether AI drafting is meaningful for this kind - the narrative fields
    /// where "write me a description / acceptance criteria" pays off. Numbers,
    /// dates, booleans and pick-lists are not draftable.
    pub fn is_draftable(self) -> bool {
        matches!(
            self,
            FieldKind::Markdown | FieldKind::PlainText | FieldKind::Text
        )
    }
}

/// One editable field of a work item, provider-normalised. The editor modal
/// renders a control from `kind`, seeds it with `value`, and (on save) sends back
/// the changed `reference` + new value. This is the single wire shape for the
/// field editor across all providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditableField {
    /// Provider field id - the Azure DevOps `referenceName`
    /// (`Microsoft.VSTS.TCM.ReproSteps`), or a synthetic id like `title` / `body`
    /// for GitHub/GitLab. Opaque to the UI; echoed back verbatim on save.
    pub reference: String,
    /// Human-friendly field name for the form label ("Repro Steps").
    pub label: String,
    /// How to render + edit it.
    pub kind: FieldKind,
    /// Current value. For `Markdown` fields this is markdown (converted from the
    /// provider's HTML where needed); other kinds are the raw string value.
    #[serde(default)]
    pub value: String,
    /// Allowed values for a `Select` field; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Read-only fields are shown for context but not editable (e.g. computed or
    /// process-locked fields). The editor greys these out.
    #[serde(default)]
    pub read_only: bool,
    /// Whether the provider marks this field required on the item's form.
    #[serde(default)]
    pub required: bool,
    /// Optional help text from the provider (Azure DevOps field `helpText`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl EditableField {
    /// Whether the AI "draft/improve" affordance should be offered for this field
    /// (a draftable kind that isn't read-only).
    pub fn allows_ai_assist(&self) -> bool {
        !self.read_only && self.kind.is_draftable()
    }
}

/// A requested change to one field: its `reference` and the new value (markdown
/// for rich fields). The wire type for the field-update endpoint/command; the
/// provider translates each into a native patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub reference: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draftable_kinds_are_the_narrative_ones() {
        assert!(FieldKind::Markdown.is_draftable());
        assert!(FieldKind::PlainText.is_draftable());
        assert!(FieldKind::Text.is_draftable());
        assert!(!FieldKind::Integer.is_draftable());
        assert!(!FieldKind::DateTime.is_draftable());
        assert!(!FieldKind::Boolean.is_draftable());
        assert!(!FieldKind::Select.is_draftable());
    }

    #[test]
    fn ai_assist_offered_only_for_editable_narrative_fields() {
        let mut f = EditableField {
            reference: "System.Description".into(),
            label: "Description".into(),
            kind: FieldKind::Markdown,
            value: String::new(),
            options: vec![],
            read_only: false,
            required: false,
            help: None,
        };
        assert!(f.allows_ai_assist());
        f.read_only = true; // a read-only rich field gets no draft button
        assert!(!f.allows_ai_assist());
        f.read_only = false;
        f.kind = FieldKind::Select; // a pick-list isn't draftable
        assert!(!f.allows_ai_assist());
    }

    #[test]
    fn field_kind_serialises_snake_case() {
        // The frontend switches on these strings; pin the wire spelling.
        assert_eq!(
            serde_json::to_string(&FieldKind::PlainText).unwrap(),
            "\"plain_text\""
        );
        assert_eq!(
            serde_json::to_string(&FieldKind::Markdown).unwrap(),
            "\"markdown\""
        );
    }

    #[test]
    fn editable_field_round_trips() {
        let f = EditableField {
            reference: "Microsoft.VSTS.Common.Priority".into(),
            label: "Priority".into(),
            kind: FieldKind::Select,
            value: "2".into(),
            options: vec!["1".into(), "2".into(), "3".into(), "4".into()],
            read_only: false,
            required: true,
            help: Some("Business priority".into()),
        };
        let back: EditableField =
            serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }
}
