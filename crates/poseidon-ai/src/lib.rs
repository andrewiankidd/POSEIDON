//! Provider-agnostic AI tag suggester.
//!
//! Given a work item (title + type + current tags) and the team's ALLOWED tag
//! set, an [`AiTagger`] proposes a subset of those tags. It mirrors the tracker
//! `Provider` pattern: a trait with a pluggable backend. The one backend today is
//! [`ChatTagger`], an OpenAI-compatible chat client - which points at a LOCAL model
//! (Ollama / LM Studio / vLLM, so a work item's title never leaves the box) or at
//! Claude / Gemini / OpenAI via their OpenAI-compatible endpoints, purely by
//! configuration (endpoint + model + key).
//!
//! Two guarantees hold regardless of backend, enforced in [`parse_suggestions`]:
//! output is **validated against the allowed set** (a model that invents a tag has
//! it dropped), and results are only ever *suggestions* - they feed the same
//! advisory [`TagSuggestion`] chips the keyword suggester populates, and nothing
//! is applied without a person clicking it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use poseidon_core::{TagSuggestion, WorkItem};
use serde::Deserialize;

mod embedded;
pub mod presets;
pub use presets::{OfflineModel, OnlineProvider, OFFLINE_MODELS, ONLINE_PROVIDERS};

/// What we send the model about one item. Deliberately minimal - just what's
/// needed to classify, so the least possible data leaves the instance.
#[derive(Debug, Clone)]
pub struct TaggerInput {
    pub id: i64,
    pub title: String,
    pub work_item_type: String,
    pub current_tags: Vec<String>,
    /// Optional body/description (plain text). Populated only when the owner opts in
    /// to feeding descriptions to the tagger; `None` = title-only (the old behaviour).
    pub description: Option<String>,
}

impl From<&WorkItem> for TaggerInput {
    fn from(w: &WorkItem) -> Self {
        Self {
            id: w.id,
            title: w.title.clone(),
            work_item_type: w.work_item_type.clone(),
            current_tags: w.tags.clone(),
            description: w.description.clone(),
        }
    }
}

/// Cap on how much of the description we put in the prompt - long bodies blow up token
/// cost/latency (and the local models' context) for little tagging gain.
pub const MAX_DESC_CHARS: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("AI backend request failed: {0}")]
    Http(String),
    #[error("{0}")]
    Unsupported(String),
}

/// Per-tag disambiguation keywords, keyed by the tag's lowercased spelling. Sourced
/// from the team's keyword rules (`tag_keywords`) and shown next to each candidate
/// so the model knows what a tag actually denotes - e.g. `area:platform-deployment`
/// keys on the phrase "platform deployment", NOT any mention of "platform" (which an
/// org that puts "platform" in everything makes ambiguous). Empty = no hints.
pub type TagHints = std::collections::HashMap<String, Vec<String>>;

/// How many keyword hints to show per tag - enough to disambiguate, capped so a
/// tag with a long keyword list doesn't blow up the prompt.
pub(crate) const MAX_HINTS_PER_TAG: usize = 6;

/// A backend that proposes tags for a work item from an allowed set.
///
/// `allowed` is the full concrete candidate set; `required` is the subset of the
/// team's required-tag PATTERNS (e.g. `["area:*", "source:*"]`). A required
/// pattern the item does not yet satisfy is a must-fill slot: the model is asked
/// to best-effort pick a value for it even on weak signal, while everything else
/// keeps the precision-first "omit when unsure" behaviour. `hints` carries optional
/// per-tag keyword glosses (see [`TagHints`]) so the model can disambiguate what
/// each candidate means.
#[async_trait]
pub trait AiTagger: Send + Sync {
    async fn suggest(
        &self,
        item: &TaggerInput,
        allowed: &[String],
        required: &[String],
        hints: &TagHints,
        background: &str,
    ) -> Result<Vec<TagSuggestion>, AiError>;

    /// Draft or improve the text of ONE work-item field, given the item's context.
    /// Returns markdown (the editor's rich-field format). Default: unsupported - only
    /// the online chat backend can generate free-form prose; the keyword/embedded
    /// backends decline so the caller can surface "connect an online model".
    async fn draft_field(&self, _ctx: &FieldDraftContext) -> Result<String, AiError> {
        Err(AiError::Unsupported(
            "AI drafting needs an online model (Settings → AI)".into(),
        ))
    }

    /// Judge ONE work item's data quality for the on-demand healthcheck, returning
    /// the concerns found (empty = clean). Default: unsupported - the keyword /
    /// embedded backends decline so the caller can fall back to the browser's WebGPU
    /// model (which runs the same prompt via the value-or-prompt handshake).
    async fn audit_item(
        &self,
        _input: &AuditInput,
        _background: &str,
    ) -> Result<Vec<AuditIssue>, AiError> {
        Err(AiError::Unsupported(
            "The AI healthcheck needs an online model (Settings → AI)".into(),
        ))
    }

    /// Rewrite all of an item's proposed fields into a mutually-consistent set in one
    /// pass. Returns `(reference, value)` for each field. Default: unsupported (only the
    /// online chat backend generates prose; the browser runs it via the handshake).
    async fn refine_fields(
        &self,
        _ctx: &FieldsConsistencyContext,
    ) -> Result<Vec<(String, String)>, AiError> {
        Err(AiError::Unsupported(
            "AI drafting needs an online model (Settings → AI)".into(),
        ))
    }
}

/// Whether to write a field from scratch or refine what's already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftMode {
    /// Author the field from the item's context (title/type/other fields).
    Draft,
    /// Improve/expand the existing value, keeping its intent.
    Improve,
}

/// Everything the model needs to draft one field well: what the item IS, what the
/// target field is, what's already there, sibling fields for context, and the team's
/// background glossary. Assembled by the service from the item + its editable fields.
#[derive(Debug, Clone)]
pub struct FieldDraftContext {
    pub work_item_type: String,
    pub title: String,
    /// Human label of the field being drafted ("Acceptance Criteria").
    pub field_label: String,
    /// The field's current markdown value (may be empty).
    pub current_value: String,
    /// Other fields (label, markdown value) for grounding - e.g. the Description when
    /// drafting Acceptance Criteria. Empty values are skipped by the prompt builder.
    pub other_fields: Vec<(String, String)>,
    /// The team background/glossary (`RuleSet.team_background`), so the model uses the
    /// team's real terminology. May be empty.
    pub background: String,
    pub mode: DraftMode,
    /// When true, an Acceptance Criteria field is written as Given-When-Then (the house
    /// style). Ignored for every other field. Defaults on via `RuleSet`.
    pub acceptance_criteria_gwt: bool,
}

/// Whether a field label denotes Acceptance Criteria (so the GWT house style applies).
/// Matches the ADO field label / GitHub-ish equivalents, case-insensitively.
pub fn is_acceptance_criteria(label: &str) -> bool {
    let l = label.trim().to_ascii_lowercase();
    l.contains("acceptance criteria") || l == "acceptance" || l == "ac"
}

/// The Given-When-Then instruction appended to the prompt for an Acceptance Criteria
/// field when the team keeps the GWT house style. Kept as one string so the per-field
/// draft and the whole-item consistency sweep phrase it identically.
pub const GWT_ACCEPTANCE_CRITERIA_INSTRUCTION: &str = "Write the Acceptance Criteria as \
Given-When-Then scenarios, with EACH clause on its OWN line. Start every line with Given, \
When, Then, And, or But, and put a blank line between clauses so they render as separate \
lines. NEVER comma-join clauses onto one line, collapse them into a prose paragraph, or use \
a bare bullet list. Format each scenario exactly like this:\n\n\
Given <context>\n\n\
When <action>\n\n\
Then <expected outcome>\n\n\
And <extra outcome>\n\n\
Use one scenario per acceptance condition, separated by a blank line.";

/// Whether a field label denotes the work item's Title (so the short-title style
/// applies). Matches the ADO/GitHub/GitLab "Title" label case-insensitively.
pub fn is_title(label: &str) -> bool {
    let l = label.trim().to_ascii_lowercase();
    l == "title" || l == "summary" || l.ends_with(" title")
}

/// The style instruction appended when drafting/improving a Title. A title is a
/// scannable backlog label, not a description - one line, no follow-on clauses.
pub const TITLE_DRAFT_INSTRUCTION: &str = "This is the work item's TITLE: return a SHORT, \
distinctive SINGLE line - one sentence (roughly 5-12 words), no trailing period. State the \
essence only; do NOT add a second sentence, parenthetical, or trailing clause like \"update \
X if necessary\" (that detail belongs in the description, not the title). It must read at a \
glance in a backlog list.";

/// System prompt for field drafting - a different job from tagging (free-form prose
/// vs. a constrained pick), so its own instruction set.
pub const FIELD_DRAFT_SYSTEM_PROMPT: &str = "You help a product owner write the fields of \
a software work item (backlog ticket). Write clear, concise, professional content for the \
ONE requested field, appropriate to the work-item type and the field's purpose (e.g. \
Acceptance Criteria = a checklist of testable conditions; Repro Steps = numbered steps + \
expected vs actual; Description = the what and why). Ground everything in the provided \
context - never invent specifics (names, ids, dates, APIs) that aren't implied by it; where \
a detail is genuinely unknown, write a clear placeholder in [square brackets] for a human to \
fill. NEVER LOSE INFORMATION: every distinct fact, link, image, attachment, step, and detail \
present in the input MUST survive into your output - you may reword, reorder, or tighten, but \
never drop content. In particular PRESERVE every existing URL, link, and inline image EXACTLY, \
character for character - keep markdown images as ![alt](url) and links as [text](url); never \
shorten, split, reword, or delete a URL, image, or attachment. Match the team's terminology \
from any TEAM BACKGROUND. NEVER write a bare URL inside square brackets or as [label: url] \
(that is not a link and renders broken). Output ONLY the field content as GitHub-flavoured \
markdown - no field name, no preamble, no code fences around the whole thing.";

/// Build the user prompt for a field draft. Pure + testable: the item context, the
/// target field, its current value, sibling fields, and the team background, framed
/// by the draft vs. improve intent.
pub fn build_field_draft_prompt(ctx: &FieldDraftContext) -> String {
    let mut p = String::new();
    if !ctx.background.trim().is_empty() {
        p.push_str("TEAM BACKGROUND:\n");
        p.push_str(ctx.background.trim());
        p.push_str("\n\n");
    }
    p.push_str(&format!(
        "WORK ITEM\n- Type: {}\n- Title: {}\n",
        ctx.work_item_type.trim(),
        ctx.title.trim()
    ));
    for (label, value) in &ctx.other_fields {
        let v = value.trim();
        if v.is_empty() {
            continue;
        }
        // Cap sibling context so one long field can't blow the token budget.
        let v: String = v.chars().take(MAX_DESC_CHARS).collect();
        p.push_str(&format!("- {}: {}\n", label.trim(), v));
    }
    p.push('\n');
    match ctx.mode {
        DraftMode::Improve if !ctx.current_value.trim().is_empty() => {
            p.push_str(&format!(
                "Improve the \"{}\" field below - fix clarity, structure and completeness while \
                 keeping its intent. Return the improved field only.\n\nCURRENT {}:\n{}",
                ctx.field_label.trim(),
                ctx.field_label.trim().to_uppercase(),
                ctx.current_value.trim()
            ));
        }
        _ => {
            p.push_str(&format!(
                "Draft the \"{}\" field for this work item from the context above. Return the \
                 field content only.",
                ctx.field_label.trim()
            ));
        }
    }
    // House style: Acceptance Criteria as Given-When-Then, unless the team opted out.
    if ctx.acceptance_criteria_gwt && is_acceptance_criteria(&ctx.field_label) {
        p.push_str("\n\n");
        p.push_str(GWT_ACCEPTANCE_CRITERIA_INSTRUCTION);
    }
    // A title is a scannable label - keep it to one short, distinctive line.
    if is_title(&ctx.field_label) {
        p.push_str("\n\n");
        p.push_str(TITLE_DRAFT_INSTRUCTION);
    }
    p
}

// ─────────────────────────────── AI healthcheck audit ───────────────────────
//
// A DIFFERENT job from tagging: rather than pick from a fixed set, the model
// JUDGES one work item's data quality and reports concrete concerns (a vague
// title, a description that contradicts the title, placeholder/malformed data).
// On-demand and advisory - the findings surface as `ai_audit` flags a person
// chose to compute, never a silent side effect of polling. Like tagging it runs
// on whichever backend is active (server online, or the browser's WebGPU model
// via the value-or-prompt handshake), so the pure prompt + parser live here and
// both paths share them.

/// What the model flagged about one item. Kept coarse + stable so the UI can
/// group/colour and the wire slug never churns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// Too vague to action - you can't tell what the work actually is.
    Unclear,
    /// The title is a placeholder or says nothing (beyond the deterministic term list).
    BadTitle,
    /// The data is internally wrong: description contradicts the title, is malformed,
    /// or is obvious nonsense / boilerplate left unfilled.
    BadData,
}

impl AuditKind {
    /// Stable wire slug (matches the serde rename).
    pub fn as_str(self) -> &'static str {
        match self {
            AuditKind::Unclear => "unclear",
            AuditKind::BadTitle => "bad_title",
            AuditKind::BadData => "bad_data",
        }
    }
    /// Parse a model-supplied kind, case/space-insensitively. Unknown -> None (dropped).
    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "unclear" | "vague" | "underspecified" => Some(AuditKind::Unclear),
            "bad_title" | "title" | "placeholder_title" => Some(AuditKind::BadTitle),
            "bad_data" | "data" | "invalid" | "contradiction" => Some(AuditKind::BadData),
            _ => None,
        }
    }
}

/// One concern the audit raised about an item: what KIND, and a one-line human
/// explanation. Both the server path and the browser path produce these via
/// [`parse_audit_response`], so a finding means the same thing wherever it ran.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditIssue {
    pub kind: AuditKind,
    /// A short, specific explanation (the "why") - shown in the flag message.
    pub detail: String,
}

/// What we send the model to audit one item. Minimal, like [`TaggerInput`].
#[derive(Debug, Clone)]
pub struct AuditInput {
    pub id: i64,
    pub title: String,
    pub work_item_type: String,
    pub state: String,
    pub description: Option<String>,
}

impl From<&WorkItem> for AuditInput {
    fn from(w: &WorkItem) -> Self {
        Self {
            id: w.id,
            title: w.title.clone(),
            work_item_type: w.work_item_type.clone(),
            state: w.state.clone(),
            description: w.description.clone(),
        }
    }
}

/// Absolute cap on audit issues kept per item - a runaway model can't dump a wall
/// of concerns. Well above the realistic count (an item has one or two real
/// problems), so it only bites pathological output.
pub const MAX_AUDIT_ISSUES: usize = 4;

pub const AUDIT_SYSTEM_PROMPT: &str = "You are a meticulous backlog-hygiene reviewer. \
You are given ONE software work item (backlog ticket). Judge ONLY its data quality - is it \
clear, self-consistent, and specific enough for someone else to pick up? Report concrete \
problems, not style nitpicks. Use these kinds: \"unclear\" (too vague to know what the work \
is), \"bad_title\" (a placeholder or uninformative title), \"bad_data\" (the description \
contradicts the title, is malformed, or is obviously boilerplate / nonsense left unfilled). \
Do NOT invent problems: a terse-but-clear item is FINE, and a well-formed item must return \
an empty list. Judge the item as it IS - never ask for more process. A resolved/closed item \
that reads clearly is fine even if brief. \
Reply with ONLY a JSON object, no prose, no code fences: \
{\"issues\":[{\"kind\":\"<unclear|bad_title|bad_data>\",\"detail\":\"<one short sentence>\"}]}. \
If nothing is wrong, reply {\"issues\":[]}.";

/// Build the user prompt for an audit. Pure + testable: item identity, type,
/// state, title, and a bounded description, plus the team background so the model
/// doesn't mistake correct internal jargon for nonsense.
pub fn build_audit_prompt(input: &AuditInput, background: &str) -> String {
    let mut out = String::new();
    let bg = background.trim();
    if !bg.is_empty() {
        out.push_str(
            "TEAM BACKGROUND (this team's systems + internal names - so you don't mistake \
             correct jargon for nonsense):\n",
        );
        out.push_str(bg);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "Work item to audit:\n- Type: {}\n- State: {}\n- Title: {}\n",
        input.work_item_type.trim(),
        input.state.trim(),
        input.title.trim(),
    ));
    match input
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(d) => {
            let text: String = d.chars().take(MAX_DESC_CHARS).collect();
            out.push_str(&format!("- Description: {text}\n"));
        }
        None => out.push_str("- Description: (empty)\n"),
    }
    out.push_str("\nReturn the JSON now.");
    out
}

#[derive(Deserialize, Default)]
struct AuditReply {
    #[serde(default)]
    issues: Vec<AuditIssueRaw>,
}

#[derive(Deserialize, Default)]
struct AuditIssueRaw {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    detail: String,
}

/// Turn a model's raw audit reply into validated issues. The trust boundary for
/// the audit: extract the JSON (models wrap it in prose / fences), keep only known
/// kinds with a non-empty detail, dedupe by (kind, detail), and cap. Pure - both
/// the server backend and the browser path are thin glue over this.
pub fn parse_audit_response(content: &str) -> Vec<AuditIssue> {
    let json = extract_json(content);
    let reply: AuditReply = serde_json::from_str(&json).unwrap_or_default();
    let mut seen = HashSet::new();
    reply
        .issues
        .into_iter()
        .filter_map(|raw| {
            let kind = AuditKind::parse(&raw.kind)?;
            let detail = raw.detail.trim();
            if detail.is_empty() {
                return None;
            }
            let key = (kind, detail.to_lowercase());
            if !seen.insert(key) {
                return None;
            }
            Some(AuditIssue {
                kind,
                detail: detail.to_string(),
            })
        })
        .take(MAX_AUDIT_ISSUES)
        .collect()
}

// ─────────────────────────── Whole-item consistency sweep ───────────────────
//
// The per-field draft (`draft_field`) writes ONE field in isolation. After a user
// drafts several, the fields can drift - the Description and the Acceptance Criteria
// use different terms, or repeat each other. This pass takes ALL the proposed fields
// at once and rewrites them into a mutually-consistent set, so the "Improve all"
// action ends with a coherent item. Advisory, like everything else: the refined
// values land in each field's review pane for the user to keep or discard.

/// One field handed to the consistency sweep: its reference, human label, and the
/// currently-proposed markdown value (from the per-field drafts, or the existing value).
#[derive(Debug, Clone)]
pub struct DraftFieldValue {
    pub reference: String,
    pub label: String,
    pub value: String,
}

/// Context for the whole-item consistency sweep: what the item IS, the team glossary,
/// and every editable rich field's proposed value, so the model harmonises them in one
/// pass rather than field-by-field.
#[derive(Debug, Clone)]
pub struct FieldsConsistencyContext {
    pub work_item_type: String,
    pub title: String,
    pub background: String,
    pub fields: Vec<DraftFieldValue>,
    /// When true, an Acceptance Criteria field among `fields` is harmonised as
    /// Given-When-Then (the house style). Defaults on via `RuleSet`.
    pub acceptance_criteria_gwt: bool,
}

pub const FIELDS_CONSISTENCY_SYSTEM_PROMPT: &str = "You are making the fields of ONE software \
work item CONSISTENT with each other. This is NOT a rewrite, polish, or clean-up pass - it is \
a minimal consistency alignment. You are given the item and several of its fields with their \
current (possibly just-drafted) content. Make ONLY the changes needed so the fields agree with \
one another: use the SAME terminology across fields, resolve any direct CONTRADICTIONS between \
fields, and keep each statement in its proper field (Description = the what and why; Repro \
Steps = numbered steps + expected vs actual; Acceptance Criteria = a checklist of testable \
conditions) rather than duplicated across fields. Otherwise leave each field's content, detail, \
wording, ordering and length EXACTLY AS-IS - do NOT reword for style, do NOT condense, shorten, \
summarise or 'improve' anything, and do NOT drop content. Do NOT invent specifics (names, ids, \
dates, APIs) not implied by the input; where a detail is unknown, keep a clear [square-bracket] \
placeholder. NEVER LOSE \
INFORMATION: every distinct fact, link, image, attachment, and step present in a field's \
input MUST survive into that field's output - de-duplicate ACROSS fields, but never \
drop a detail entirely. PRESERVE every existing URL, link, and inline image EXACTLY - keep \
markdown images as ![alt](url) and links as [text](url), never shorten, reword, or delete a \
URL, image, or attachment, and never move an image out of the field it belongs to. Match the \
team's terminology from any TEAM BACKGROUND; write links only as valid markdown, never as a \
bare URL in brackets. Return \
EVERY field you were given, each as GitHub-flavoured markdown, keyed by its exact reference. \
Reply with ONLY a JSON object, no prose, no code fences: \
{\"fields\":[{\"reference\":\"<the field reference>\",\"value\":\"<markdown>\"}]}.";

/// Build the user prompt for the consistency sweep. Pure + testable.
pub fn build_fields_consistency_prompt(ctx: &FieldsConsistencyContext) -> String {
    let mut p = String::new();
    let bg = ctx.background.trim();
    if !bg.is_empty() {
        p.push_str("TEAM BACKGROUND:\n");
        p.push_str(bg);
        p.push_str("\n\n");
    }
    p.push_str(&format!(
        "WORK ITEM\n- Type: {}\n- Title: {}\n\nFIELDS TO HARMONISE:\n",
        ctx.work_item_type.trim(),
        ctx.title.trim()
    ));
    let mut has_ac = false;
    for f in &ctx.fields {
        // Cap each field so one long body can't blow the budget.
        let v: String = f.value.trim().chars().take(MAX_DESC_CHARS).collect();
        let shown = if v.is_empty() { "(empty)" } else { &v };
        p.push_str(&format!(
            "\n--- reference: {} | label: {} ---\n{}\n",
            f.reference.trim(),
            f.label.trim(),
            shown
        ));
        has_ac |= is_acceptance_criteria(&f.label);
    }
    // House style: Acceptance Criteria as Given-When-Then, unless the team opted out.
    if ctx.acceptance_criteria_gwt && has_ac {
        p.push('\n');
        p.push_str(GWT_ACCEPTANCE_CRITERIA_INSTRUCTION);
    }
    p.push_str("\nReturn the JSON now, one entry per field above.");
    p
}

#[derive(Deserialize, Default)]
struct ConsistencyReply {
    #[serde(default)]
    fields: Vec<ConsistencyField>,
}

#[derive(Deserialize, Default)]
struct ConsistencyField {
    #[serde(default)]
    reference: String,
    #[serde(default)]
    value: String,
}

/// Parse a consistency reply into `(reference, value)` pairs. The trust boundary:
/// extract the JSON, keep only fields whose reference was in the request (canonical
/// spelling preserved), drop empties, dedupe by reference. Pure - both the server and
/// the browser path share it.
pub fn parse_fields_consistency(content: &str, known_refs: &[String]) -> Vec<(String, String)> {
    let json = extract_json(content);
    let reply: ConsistencyReply = serde_json::from_str(&json).unwrap_or_default();
    let canon: HashMap<String, &String> =
        known_refs.iter().map(|r| (r.to_lowercase(), r)).collect();
    let mut seen = HashSet::new();
    reply
        .fields
        .into_iter()
        .filter_map(|f| {
            let key = f.reference.trim().to_lowercase();
            let canonical = canon.get(&key)?; // drop refs not in the request
            let value = strip_code_fence(f.value.trim());
            if value.trim().is_empty() {
                return None;
            }
            if !seen.insert(canonical.to_lowercase()) {
                return None;
            }
            Some(((*canonical).clone(), value))
        })
        .collect()
}

/// Instance-level AI config: which backend suggests tags. Persisted (set via
/// onboarding / settings) or seeded from env. Serialisable so it round-trips
/// through the store and the config API.
///
///   mode = "online"  -> a hosted provider (Claude / ChatGPT / Gemini, or a
///                       custom OpenAI-compatible endpoint) + an API key.
///   mode = "offline" -> a small model POSEIDON downloads and runs in-process
///                       (no Ollama). Backed by the embedded engine.
///   anything else    -> off (only the keyword suggester runs).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub mode: String,
    /// online: a provider id (see [`ONLINE_PROVIDERS`]) or "custom".
    #[serde(default)]
    pub provider: Option<String>,
    /// online: OpenAI-compatible endpoint (only when provider = "custom").
    #[serde(default)]
    pub endpoint: Option<String>,
    /// online: model override (defaults to the provider's default_model).
    #[serde(default)]
    pub model: Option<String>,
    /// online: bearer API key. Stored server-side; redacted before it's returned
    /// to the browser (see the config API).
    #[serde(default)]
    pub api_key: Option<String>,
    /// offline: a preset model id (see [`OFFLINE_MODELS`]).
    #[serde(default)]
    pub offline_model: Option<String>,
}

impl AiConfig {
    /// Back-compat: `POSEIDON_AI_ENDPOINT`/`_MODEL`/`_API_KEY` map to a custom
    /// online provider, so an env-configured deployment (e.g. the bundled Ollama
    /// via the chart) keeps working. Otherwise off.
    pub fn from_env() -> Self {
        let endpoint = env_nonempty("POSEIDON_AI_ENDPOINT");
        let model = env_nonempty("POSEIDON_AI_MODEL");
        if endpoint.is_some() && model.is_some() {
            Self {
                mode: "online".to_string(),
                provider: Some("custom".to_string()),
                endpoint,
                model,
                api_key: env_nonempty("POSEIDON_AI_API_KEY"),
                offline_model: None,
            }
        } else {
            Self::default()
        }
    }

    /// Build the backend, or `None` when off / unconfigured.
    pub fn build(&self) -> Option<Arc<dyn AiTagger>> {
        match self.mode.as_str() {
            "online" => {
                let (endpoint, model) = self.resolve_online()?;
                Some(Arc::new(ChatTagger {
                    http: reqwest::Client::new(),
                    endpoint,
                    model,
                    api_key: self.api_key.clone(),
                }))
            }
            // "offline" is wired to the embedded engine in embedded.rs.
            "offline" => self.build_offline(),
            _ => None,
        }
    }

    /// Build the offline (in-process) backend. Landing point for the embedded
    /// engine (candle): download+cache the preset GGUF and run it locally. Until
    /// that ships, offline configs are stored but inactive (returns `None`), so
    /// the app cleanly falls back to "no AI" rather than erroring.
    fn build_offline(&self) -> Option<Arc<dyn AiTagger>> {
        let preset = presets::offline_model(self.offline_model.as_deref()?)?;
        Some(Arc::new(embedded::EmbeddedTagger::new(preset)))
    }

    /// Resolve the online endpoint + model from the provider preset (or custom).
    fn resolve_online(&self) -> Option<(String, String)> {
        match self.provider.as_deref().unwrap_or("custom") {
            "custom" => Some((self.endpoint.clone()?, self.model.clone()?)),
            id => {
                let p = presets::online_provider(id)?;
                let model = self
                    .model
                    .clone()
                    .unwrap_or_else(|| p.default_model.to_string());
                Some((p.endpoint.to_string(), model))
            }
        }
    }

    /// Whether a backend would be built (for the doctor / status surfaces).
    pub fn enabled(&self) -> bool {
        match self.mode.as_str() {
            "online" => self.resolve_online().is_some(),
            "offline" => self
                .offline_model
                .as_deref()
                .and_then(presets::offline_model)
                .is_some(),
            _ => false,
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ─────────────────────────── LLM integration registry ───────────────────────
//
// An owner configures MANY integrations; the effective one is the first in
// priority order that is compatible with the current platform (desktop with a
// GPU picks a GPU-offline entry; the CPU web pod falls through to an online /
// custom-endpoint entry; a browser client can run a WebGPU entry). One codebase,
// one config surface, several execution engines - each user picks what fits.

/// What the current runtime can actually do. The server fills this for its own
/// resolution; the browser client computes its own (for WebGPU) on the frontend.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct PlatformCaps {
    /// Can run the in-process embedded engine (candle). False for a browser client.
    #[serde(default)]
    pub embedded: bool,
    /// A CUDA GPU is available to the embedded engine (cuda build + a live device).
    #[serde(default)]
    pub gpu: bool,
    /// Can run a model in-browser via WebGPU (client-side only).
    #[serde(default)]
    pub webgpu: bool,
    /// Best-effort GPU/accelerator VRAM in MB, when the platform can report it (a
    /// native CUDA probe; WebGPU can't expose total VRAM, so this stays None there
    /// and the WebGPU tier falls back to a safe default). Drives model sizing.
    #[serde(default)]
    pub vram_mb: Option<u32>,
    /// Best-effort system RAM in MB (browser `deviceMemory` is coarse + capped).
    #[serde(default)]
    pub ram_mb: Option<u32>,
    /// Logical CPU cores, when known. Sizes the CPU-only model tier.
    #[serde(default)]
    pub cpu_cores: Option<u32>,
}

impl PlatformCaps {
    /// This server process's capabilities (pod / desktop / CLI).
    pub fn server() -> Self {
        Self {
            embedded: true,
            gpu: embedded::cuda_available(),
            webgpu: false,
            vram_mb: None,
            ram_mb: None,
            cpu_cores: std::thread::available_parallelism()
                .ok()
                .map(|n| n.get() as u32),
        }
    }

    /// Merge browser-supplied caps over this (server) set: the browser is the only
    /// place that knows WebGPU availability + client RAM/cores, so those win; the
    /// server keeps its own embedded/gpu truth. Used by the autotune endpoint.
    pub fn merged_with_browser(self, browser: &PlatformCaps) -> Self {
        Self {
            embedded: self.embedded,
            gpu: self.gpu || browser.gpu,
            webgpu: browser.webgpu,
            vram_mb: browser.vram_mb.or(self.vram_mb),
            ram_mb: browser.ram_mb.or(self.ram_mb),
            cpu_cores: browser.cpu_cores.or(self.cpu_cores),
        }
    }
}

/// The best offline model id to run for an integration of `kind`/`device` given the
/// platform's `caps` - "highest reasonably runnable", so a strong box gets a strong
/// model out of the box and a weak one stays fast. VRAM tiers apply when a real
/// number is known; WebGPU can't report total VRAM, so it defaults to a safe strong
/// model (3B, ~2GB) that any discrete GPU handles - the benchmark "tune" is the
/// reliable path higher. See [`OFFLINE_MODELS`] for the ids.
pub fn recommend_model(kind: &str, device: &str, caps: &PlatformCaps) -> &'static str {
    // Catalog-driven: pick the LARGEST auto-eligible model whose footprint fits the
    // available VRAM. `caps.vram_mb` is measured-allocatable on WebGPU (the browser probe)
    // and total on CUDA; either way we compare to each model's `min_vram_mb`. To change
    // what gets selected - or swap the whole model family - edit OFFLINE_MODELS, not this.
    let largest_fit = |vram: u32, webgpu_only: bool| -> Option<&'static str> {
        OFFLINE_MODELS
            .iter()
            .filter(|m| m.auto && (!webgpu_only || !m.webgpu_id.is_empty()))
            .filter(|m| m.min_vram_mb <= vram)
            .max_by_key(|m| m.min_vram_mb)
            .map(|m| m.id)
    };
    let smallest = |webgpu_only: bool| -> &'static str {
        OFFLINE_MODELS
            .iter()
            .filter(|m| !webgpu_only || !m.webgpu_id.is_empty())
            .min_by_key(|m| m.min_vram_mb)
            .map(|m| m.id)
            .unwrap_or_else(|| presets::smallest_auto_model().id)
    };
    // VRAM unmeasured: the catalog's balanced default (the load-fallback ladder tunes it
    // down if it won't fit). Every id below comes from the catalog - none is a literal.
    let balanced_default = presets::default_auto_model().id;
    let smallest_any = presets::smallest_auto_model().id;
    match kind {
        "webgpu" => match caps.vram_mb {
            Some(v) => largest_fit(v, true).unwrap_or_else(|| smallest(true)),
            None => balanced_default,
        },
        "offline" if device == "gpu" => match caps.vram_mb {
            Some(v) => largest_fit(v, false).unwrap_or_else(|| smallest(false)),
            None => balanced_default,
        },
        "offline" => match caps.cpu_cores {
            Some(c) if c >= 8 => balanced_default,
            _ => smallest_any,
        },
        _ => smallest_any,
    }
}

/// One configured backend in an owner's registry. Reuses the `AiConfig` connection
/// fields (provider/model/endpoint/key) + identity, kind, and device preference.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmIntegration {
    pub id: String,
    pub name: String,
    /// "offline" (embedded candle) · "online" (HTTP: cloud or custom endpoint) ·
    /// "webgpu" (in-browser, client-side - stored + gated now; engine ships later).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub offline_model: Option<String>,
    /// offline: "gpu" to prefer CUDA, else "cpu" (drives compatibility + priority).
    #[serde(default)]
    pub device: String,
}

impl LlmIntegration {
    /// Map to the connection-level `AiConfig` the tagger backends already build from.
    fn to_ai_config(&self) -> AiConfig {
        let mode = match self.kind.as_str() {
            "offline" => "offline",
            "online" => "online",
            _ => "off", // webgpu builds no server-side tagger
        };
        AiConfig {
            mode: mode.to_string(),
            provider: self.provider.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            offline_model: self.offline_model.clone(),
        }
    }

    /// Whether this integration CAN run on a platform with `caps`.
    pub fn compatible(&self, caps: &PlatformCaps) -> bool {
        match self.kind.as_str() {
            "online" => true, // an HTTP endpoint runs anywhere with a network
            "offline" => caps.embedded && (self.device != "gpu" || caps.gpu),
            "webgpu" => caps.webgpu,
            _ => false,
        }
    }

    /// Whether the integration is filled in enough to run (not whether it'll succeed).
    /// A hosted cloud preset additionally needs an API key - without one it's a
    /// template, not a usable backend (so it never resolves as "active"). A custom
    /// endpoint (local Ollama/LM Studio) needs no key.
    pub fn configured(&self) -> bool {
        match self.kind.as_str() {
            "online" => {
                if !self.to_ai_config().enabled() {
                    return false;
                }
                let is_custom = self.provider.as_deref().is_none_or(|p| p == "custom");
                is_custom
                    || self
                        .api_key
                        .as_deref()
                        .is_some_and(|k| !k.trim().is_empty())
            }
            "offline" => self.to_ai_config().enabled(),
            "webgpu" => self.model.is_some() || self.offline_model.is_some(),
            _ => false,
        }
    }

    /// Build the server-side tagger (None for webgpu - handled by the browser).
    pub fn build(&self) -> Option<std::sync::Arc<dyn AiTagger>> {
        self.to_ai_config().build()
    }
}

/// An owner's ordered registry of integrations (order = priority).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub integrations: Vec<LlmIntegration>,
    /// True when this registry was auto-configured for the platform (by the autotune
    /// endpoint) rather than hand-edited. Auto registries may be re-tuned as the
    /// detected capabilities change; the moment a user saves from Settings it flips
    /// false and is never machine-touched again.
    #[serde(default)]
    pub auto: bool,
}

impl LlmConfig {
    /// Wrap a single legacy `AiConfig` into a one-integration registry (migration
    /// path from the old single-config storage). An "off" config yields an empty
    /// registry. Keeps the old single-provider setup working through the new
    /// resolution machinery until the multi-integration UI writes a real registry.
    pub fn from_single(cfg: AiConfig) -> Self {
        if cfg.mode != "online" && cfg.mode != "offline" {
            return Self::default();
        }
        Self {
            integrations: vec![LlmIntegration {
                id: "default".to_string(),
                name: "Default".to_string(),
                kind: cfg.mode.clone(),
                provider: cfg.provider,
                endpoint: cfg.endpoint,
                model: cfg.model,
                api_key: cfg.api_key,
                offline_model: cfg.offline_model,
                device: "cpu".to_string(),
            }],
            auto: false,
        }
    }

    /// The default catalog a fresh owner sees: the full multiplatform range, ordered
    /// most→least preferred with a bias toward LOCAL (private, no per-call cost). The
    /// first entry compatible with the current platform becomes active; the rest stay
    /// visible (greyed where unusable here) so the user can reorder or add a key. The
    /// cloud entries ship keyless (templates until configured); the Ollama entry uses
    /// the canonical localhost endpoint (edit to a host/pod-reachable one as needed).
    pub fn seeded() -> Self {
        let offline = |id: &str, name: &str, model: &str, device: &str| LlmIntegration {
            id: id.to_string(),
            name: name.to_string(),
            kind: "offline".to_string(),
            offline_model: Some(model.to_string()),
            device: device.to_string(),
            ..Default::default()
        };
        let cloud = |id: &str, name: &str, provider: &str| LlmIntegration {
            id: id.to_string(),
            name: name.to_string(),
            kind: "online".to_string(),
            provider: Some(provider.to_string()),
            ..Default::default()
        };
        Self {
            integrations: vec![
                // Local-first, but ordered by real throughput: native CUDA on top, then
                // the browser's own GPU (WebGPU) - which beats a server CPU handily, the
                // case a hosted web user actually hits - then CPU as the slow fallback.
                offline(
                    "local-gpu",
                    "On-device GPU (CUDA)",
                    presets::default_auto_model().id,
                    "gpu",
                ),
                LlmIntegration {
                    id: "webgpu".to_string(),
                    name: "In-browser (WebGPU)".to_string(),
                    kind: "webgpu".to_string(),
                    offline_model: Some(presets::default_auto_model().id.to_string()),
                    ..Default::default()
                },
                offline(
                    "local-cpu",
                    "On-device CPU",
                    presets::smallest_auto_model().id,
                    "cpu",
                ),
                LlmIntegration {
                    id: "ollama".to_string(),
                    name: "Local Ollama".to_string(),
                    kind: "online".to_string(),
                    provider: Some("custom".to_string()),
                    endpoint: Some("http://localhost:11434/v1/chat/completions".to_string()),
                    // Illustrative Ollama tag (Ollama's own naming, not our catalog id) - the
                    // user edits it to whatever they've pulled; left literal on purpose.
                    model: Some("qwen3:1.7b".to_string()),
                    ..Default::default()
                },
                cloud("claude", "Claude (Anthropic)", "anthropic"),
                cloud("gemini", "Gemini (Google)", "gemini"),
                cloud("openai", "ChatGPT (OpenAI)", "openai"),
            ],
            auto: false,
        }
    }

    /// The seeded catalog, but with each local backend's model sized to `caps` via
    /// [`recommend_model`] - so a fresh owner on a strong box gets a strong model out
    /// of the box and a weak one stays fast. Marked `auto` so it can be re-tuned until
    /// the user hand-edits. Order (priority) is unchanged from [`Self::seeded`].
    pub fn seeded_for(caps: &PlatformCaps) -> Self {
        let mut cfg = Self::seeded();
        for integ in &mut cfg.integrations {
            match integ.kind.as_str() {
                "offline" | "webgpu" => {
                    integ.offline_model =
                        Some(recommend_model(&integ.kind, &integ.device, caps).to_string());
                }
                _ => {}
            }
        }
        cfg.auto = true;
        cfg
    }

    /// Heal integrations whose stored `offline_model` is no longer in the catalog
    /// (e.g. after a model-family swap like Qwen2.5 → Qwen3): coerce the dead id to the
    /// current [`recommend_model`] pick for that backend + platform. This is NOT an
    /// alias table - "unknown" simply means "invalid", and invalid resolves to the best
    /// current model, so it stays correct across any future catalog change with no
    /// per-id mappings to maintain. Leaves valid ids (the user's real choice) untouched.
    /// Returns true if anything changed, so the caller can re-persist the healed config.
    pub fn normalize_models(&mut self, caps: &PlatformCaps) -> bool {
        let mut changed = false;
        for integ in &mut self.integrations {
            if !matches!(integ.kind.as_str(), "offline" | "webgpu") {
                continue;
            }
            if let Some(id) = integ.offline_model.as_deref() {
                if presets::offline_model(id).is_none() {
                    integ.offline_model =
                        Some(recommend_model(&integ.kind, &integ.device, caps).to_string());
                    changed = true;
                }
            }
        }
        changed
    }

    /// The effective tagger for this platform: the first compatible + configured
    /// integration (in priority order) that actually builds. None = no AI here.
    pub fn resolve(&self, caps: &PlatformCaps) -> Option<std::sync::Arc<dyn AiTagger>> {
        self.integrations
            .iter()
            .filter(|i| i.compatible(caps) && i.configured())
            .find_map(|i| i.build())
    }

    /// The id of the integration that is (or would be) active on this platform -
    /// for the UI to badge it. `webgpu` entries count here even though the server
    /// can't build them, so the browser can show which one it will run.
    pub fn active_id(&self, caps: &PlatformCaps) -> Option<&str> {
        self.integrations
            .iter()
            .find(|i| i.compatible(caps) && i.configured())
            .map(|i| i.id.as_str())
    }
}

pub(crate) const SYSTEM_PROMPT: &str = "You tag software work items for a backlog-hygiene tool. \
You are given one work item, a list of ALLOWED tags, and possibly some REQUIRED \
categories. For each REQUIRED category you MUST choose exactly ONE value from the \
options listed for it - make your BEST GUESS from the title/type/description even \
when you are not fully certain; a reasonable guess is better than leaving a required \
category empty, so never skip one. List the required picks FIRST. For all OTHER \
(optional) ALLOWED tags, favour precision: add one only when it CLEARLY applies, and \
prefer returning fewer (or none) over a long list. Never pick tags that contradict \
each other (e.g. two different types like bug and enhancement, or opposites like \
internal and external). Some tags show example keywords in parentheses - e.g. \
`area:foo (e.g. bar, baz)` - which clarify what that tag MEANS; use them to pick \
the right tag, but output ONLY the tag itself (`area:foo`), never the examples. \
Never invent tags or use any tag not in the ALLOWED list. \
Reply with ONLY a JSON object, no prose, no code fences: \
{\"tags\": [\"<allowed tag>\", ...], \"rationale\": \"<one short sentence>\"}. \
If there are no required categories and nothing else clearly applies, reply \
{\"tags\": [], \"rationale\": \"none apply\"}.";

/// Absolute backstop on suggestions per item, enforced in [`parse_suggestions`]
/// regardless of what the model returns - a guard against a runaway model dumping the
/// whole allowed set. The REAL, tunable ceiling is applied downstream by the caller
/// (the service / the browser) from the ruleset's `max_suggestions`; this is only the
/// last-resort limit, set well above any sane per-item tag count.
pub(crate) const MAX_SUGGESTIONS: usize = 20;

/// Slack added over the required-axis count to size the default suggestion ceiling -
/// room for extra `area:` values, a rewrite, and a few optional tags beyond the
/// required product/area/source picks. Chosen so a 3-axis taxonomy defaults to 10.
pub const SUGGESTION_SLACK: usize = 7;

/// The adaptive default suggestion ceiling when a ruleset doesn't set `max_suggestions`:
/// enough to fit every required category plus [`SUGGESTION_SLACK`], floored so a team
/// with few/no required tags still gets a usable ceiling. Scales with the taxonomy so
/// adding a required axis never silently truncates its own picks.
pub fn default_max_suggestions(required_len: usize) -> usize {
    required_len.saturating_add(SUGGESTION_SLACK).max(6)
}

/// Match a tag against a required-tag pattern: trailing `*` is a prefix wildcard,
/// otherwise exact - both case-insensitive. (poseidon-ai doesn't depend on
/// poseidon-rules, so this mirrors `poseidon_rules::tag_matches` in miniature.)
fn pattern_matches(pattern: &str, tag: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let t = tag.trim().to_ascii_lowercase();
    match p.strip_suffix('*') {
        Some(prefix) => t.starts_with(prefix),
        None => p == t,
    }
}

/// Render a candidate tag, appending its keyword hints as `tag (e.g. kw1, kw2)`
/// when the team configured any - the disambiguation signal that stops the model
/// grabbing a tag on a surface-word match.
fn annotate(tag: &str, hints: &TagHints) -> String {
    match hints.get(&tag.to_lowercase()) {
        Some(kw) if !kw.is_empty() => {
            let shown = kw
                .iter()
                .filter(|k| !k.trim().is_empty())
                .take(MAX_HINTS_PER_TAG)
                .map(|k| k.trim())
                .collect::<Vec<_>>()
                .join(", ");
            if shown.is_empty() {
                tag.to_string()
            } else {
                format!("{tag} (e.g. {shown})")
            }
        }
        _ => tag.to_string(),
    }
}

/// The user message: the item, the tags it must still be given (grouped by
/// required category, each with its concrete options), and the remaining optional
/// tags. `required` holds the team's required-tag patterns; a pattern the item
/// already satisfies is dropped (no need to ask), and one with no options left in
/// `allowed` is skipped (nothing to offer). Everything in `allowed` that isn't
/// claimed by a listed required category is offered as optional. `hints` annotates
/// each candidate with its keyword gloss.
pub fn build_prompt(
    input: &TaggerInput,
    allowed: &[String],
    required: &[String],
    hints: &TagHints,
    background: &str,
) -> String {
    let current = if input.current_tags.is_empty() {
        "(none)".to_string()
    } else {
        input.current_tags.join(", ")
    };

    // Required categories still to fill: an unsatisfied required pattern plus the
    // allowed values that match it. Keep the pattern's own order.
    let mut required_blocks: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    for pat in required {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        // Already satisfied by a current tag? Then it's not a must-fill slot.
        if input.current_tags.iter().any(|t| pattern_matches(pat, t)) {
            continue;
        }
        let options: Vec<&String> = allowed.iter().filter(|a| pattern_matches(pat, a)).collect();
        if options.is_empty() {
            continue; // nothing to offer for this slot
        }
        for o in &options {
            claimed.insert(o.to_lowercase());
        }
        let label = pat.strip_suffix('*').unwrap_or(pat);
        let opts = options
            .iter()
            .map(|o| annotate(o, hints))
            .collect::<Vec<_>>()
            .join(", ");
        required_blocks.push(format!("- {label} (choose one): {opts}"));
    }

    // Optional = everything in allowed not already claimed by a required category.
    let optional: Vec<String> = allowed
        .iter()
        .filter(|a| !claimed.contains(&a.to_lowercase()))
        .map(|a| format!("- {}", annotate(a, hints)))
        .collect();

    // Body/description, if the owner opted in - trimmed + truncated to keep the prompt
    // bounded. Omitted entirely when absent (title-only, the old behaviour).
    let description = input
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| {
            let text: String = d.chars().take(MAX_DESC_CHARS).collect();
            format!("\n- Description: {text}")
        })
        .unwrap_or_default();

    let mut out = String::new();
    let bg = background.trim();
    if !bg.is_empty() {
        out.push_str(
            "TEAM BACKGROUND (this team's systems + internal names - use it to interpret the item):\n",
        );
        out.push_str(bg);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "Work item:\n- Title: {}\n- Type: {}{}\n- Current tags: {}\n",
        input.title, input.work_item_type, description, current
    ));
    if !required_blocks.is_empty() {
        out.push_str(
            "\nREQUIRED categories - pick exactly ONE value for EACH (best guess even if unsure):\n",
        );
        out.push_str(&required_blocks.join("\n"));
        out.push('\n');
    }
    if !optional.is_empty() {
        let header = if required_blocks.is_empty() {
            "\nALLOWED tags:\n"
        } else {
            "\nOPTIONAL tags (only if they CLEARLY apply):\n"
        };
        out.push_str(header);
        out.push_str(&optional.join("\n"));
        out.push('\n');
    }
    out.push_str("\nReturn the JSON now.");
    out
}

#[derive(Deserialize, Default)]
struct ModelReply {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    rationale: Option<String>,
}

/// Turn a model's raw reply into validated suggestions. This is the trust
/// boundary: it extracts the JSON (models often wrap it in prose / code fences)
/// and keeps ONLY tags that exist in `allowed` (case-insensitive, returned in the
/// allowed set's canonical spelling), de-duplicated. Anything invented is dropped.
/// Pure - the backend is thin glue over this.
pub fn parse_suggestions(content: &str, allowed: &[String]) -> Vec<TagSuggestion> {
    let json = extract_json(content);
    let reply: ModelReply = serde_json::from_str(&json).unwrap_or_default();
    let rationale = reply
        .rationale
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let canon: HashMap<String, &String> = allowed.iter().map(|t| (t.to_lowercase(), t)).collect();
    let mut seen = HashSet::new();
    reply
        .tags
        .into_iter()
        .filter_map(|t| {
            let key = t.trim().to_lowercase();
            let canonical = canon.get(&key)?; // drop anything not in the allowed set
            if !seen.insert(canonical.to_lowercase()) {
                return None; // de-dup
            }
            Some(TagSuggestion {
                tag: (*canonical).clone(),
                reasons: vec![rationale
                    .clone()
                    .unwrap_or_else(|| "AI suggestion".to_string())],
                replaces: None,
            })
        })
        .take(MAX_SUGGESTIONS) // backstop: keep only the first few even if the model over-tags
        .collect()
}

/// Pull the first JSON object out of a reply that may be fenced or prose-wrapped.
fn extract_json(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('{') && t.ends_with('}') {
        return t.to_string();
    }
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => t[a..=b].to_string(),
        _ => t.to_string(),
    }
}

/// OpenAI-compatible chat backend (local Ollama or a hosted compat endpoint).
struct ChatTagger {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

#[async_trait]
impl AiTagger for ChatTagger {
    async fn suggest(
        &self,
        item: &TaggerInput,
        allowed: &[String],
        required: &[String],
        hints: &TagHints,
        background: &str,
    ) -> Result<Vec<TagSuggestion>, AiError> {
        if allowed.is_empty() {
            return Ok(vec![]); // nothing to choose from
        }
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "stream": false,
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": build_prompt(item, allowed, required, hints, background) },
            ],
        });
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| AiError::Http(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");

        let mut suggestions = parse_suggestions(content, allowed);
        // Never re-suggest a tag the item already has.
        let have: HashSet<String> = item.current_tags.iter().map(|t| t.to_lowercase()).collect();
        suggestions.retain(|s| !have.contains(&s.tag.to_lowercase()));
        Ok(suggestions)
    }

    async fn draft_field(&self, ctx: &FieldDraftContext) -> Result<String, AiError> {
        let body = serde_json::json!({
            "model": self.model,
            // A touch of temperature: prose, not a deterministic tag pick.
            "temperature": 0.3,
            "stream": false,
            "messages": [
                { "role": "system", "content": FIELD_DRAFT_SYSTEM_PROMPT },
                { "role": "user", "content": build_field_draft_prompt(ctx) },
            ],
        });
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| AiError::Http(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        // Strip a stray ```markdown fence the model may wrap the whole answer in.
        Ok(strip_code_fence(&content))
    }

    async fn audit_item(
        &self,
        input: &AuditInput,
        background: &str,
    ) -> Result<Vec<AuditIssue>, AiError> {
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0, // a judgement, not prose - deterministic
            "stream": false,
            "messages": [
                { "role": "system", "content": AUDIT_SYSTEM_PROMPT },
                { "role": "user", "content": build_audit_prompt(input, background) },
            ],
        });
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| AiError::Http(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        Ok(parse_audit_response(content))
    }

    async fn refine_fields(
        &self,
        ctx: &FieldsConsistencyContext,
    ) -> Result<Vec<(String, String)>, AiError> {
        let refs: Vec<String> = ctx.fields.iter().map(|f| f.reference.clone()).collect();
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0.2,
            "stream": false,
            "messages": [
                { "role": "system", "content": FIELDS_CONSISTENCY_SYSTEM_PROMPT },
                { "role": "user", "content": build_fields_consistency_prompt(ctx) },
            ],
        });
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| AiError::Http(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        Ok(parse_fields_consistency(content, &refs))
    }
}

/// Remove a single outer code fence (```lang … ```) if the model wrapped its whole
/// reply in one - common with some chat models despite the instruction not to.
fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop the optional language token on the first line, then a trailing ```
        let after_lang = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
        if let Some(inner) = after_lang.trim_end().strip_suffix("```") {
            return inner.trim().to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec![
            "type:bug".into(),
            "type:task".into(),
            "priority:high".into(),
        ]
    }

    fn draft_ctx(mode: DraftMode) -> FieldDraftContext {
        FieldDraftContext {
            work_item_type: "User Story".into(),
            title: "Self-service portal access request".into(),
            field_label: "Acceptance Criteria".into(),
            current_value: String::new(),
            other_fields: vec![
                (
                    "Description".into(),
                    "A user can request access via the portal.".into(),
                ),
                ("System Info".into(), "   ".into()), // blank -> skipped
            ],
            background: "Portal = our internal developer portal (Backstage).".into(),
            mode,
            acceptance_criteria_gwt: true,
        }
    }

    #[test]
    fn draft_prompt_includes_context_and_skips_blank_fields() {
        let p = build_field_draft_prompt(&draft_ctx(DraftMode::Draft));
        assert!(p.contains("TEAM BACKGROUND"));
        assert!(p.contains("Backstage")); // background flows in
        assert!(p.contains("Type: User Story"));
        assert!(p.contains("Title: Self-service portal access request"));
        assert!(p.contains("Description: A user can request access")); // sibling field
        assert!(!p.contains("System Info")); // blank sibling skipped
        assert!(p.contains("Draft the \"Acceptance Criteria\""));
    }

    #[test]
    fn draft_prompt_improve_mode_carries_the_current_value() {
        let mut ctx = draft_ctx(DraftMode::Improve);
        ctx.current_value = "user can log in".into();
        let p = build_field_draft_prompt(&ctx);
        assert!(p.contains("Improve the \"Acceptance Criteria\""));
        assert!(p.contains("user can log in"));
        // Improve with an EMPTY current value falls back to draft-from-scratch.
        ctx.current_value = "  ".into();
        assert!(build_field_draft_prompt(&ctx).contains("Draft the \"Acceptance Criteria\""));
    }

    #[test]
    fn draft_prompt_adds_gwt_only_for_acceptance_criteria_when_enabled() {
        // AC field + GWT on -> the Given-When-Then instruction is appended.
        let mut ctx = draft_ctx(DraftMode::Draft); // field_label = "Acceptance Criteria"
        assert!(build_field_draft_prompt(&ctx).contains("Given-When-Then"));
        // Opted out -> no GWT instruction.
        ctx.acceptance_criteria_gwt = false;
        assert!(!build_field_draft_prompt(&ctx).contains("Given-When-Then"));
        // A different field never gets the AC instruction, even with GWT on.
        ctx.acceptance_criteria_gwt = true;
        ctx.field_label = "Repro Steps".into();
        assert!(!build_field_draft_prompt(&ctx).contains("Given-When-Then"));
    }

    #[test]
    fn is_acceptance_criteria_matches_common_spellings() {
        assert!(is_acceptance_criteria("Acceptance Criteria"));
        assert!(is_acceptance_criteria("  acceptance criteria  "));
        assert!(is_acceptance_criteria("AC"));
        assert!(!is_acceptance_criteria("Repro Steps"));
        assert!(!is_acceptance_criteria("Description"));
    }

    #[test]
    fn consistency_prompt_adds_gwt_when_an_ac_field_is_present() {
        let mut ctx = consistency_ctx(); // Repro Steps + Expected behaviour (no AC)
        assert!(!build_fields_consistency_prompt(&ctx).contains("Given-When-Then"));
        ctx.fields.push(DraftFieldValue {
            reference: "Microsoft.VSTS.Common.AcceptanceCriteria".into(),
            label: "Acceptance Criteria".into(),
            value: "user can log in".into(),
        });
        assert!(build_fields_consistency_prompt(&ctx).contains("Given-When-Then"));
        ctx.acceptance_criteria_gwt = false;
        assert!(!build_fields_consistency_prompt(&ctx).contains("Given-When-Then"));
    }

    #[test]
    fn draft_prompt_adds_short_title_instruction_for_title_field() {
        let mut ctx = draft_ctx(DraftMode::Draft);
        ctx.field_label = "Title".into();
        let p = build_field_draft_prompt(&ctx);
        assert!(p.contains("SINGLE line"));
        assert!(p.contains("do NOT add a second sentence"));
        // A non-title field never gets the short-title instruction.
        ctx.field_label = "Description".into();
        assert!(!build_field_draft_prompt(&ctx).contains("SINGLE line"));
    }

    #[test]
    fn is_title_matches_common_labels() {
        assert!(is_title("Title"));
        assert!(is_title("  title  "));
        assert!(is_title("Summary"));
        assert!(!is_title("Description"));
        assert!(!is_title("Repro Steps"));
    }

    #[test]
    fn draft_prompt_omits_background_block_when_empty() {
        let mut ctx = draft_ctx(DraftMode::Draft);
        ctx.background = "  ".into();
        assert!(!build_field_draft_prompt(&ctx).contains("TEAM BACKGROUND"));
    }

    // ── audit prompt + parser ────────────────────────────────────────────────

    fn audit_input(title: &str, state: &str, desc: Option<&str>) -> AuditInput {
        AuditInput {
            id: 7,
            title: title.into(),
            work_item_type: "Bug".into(),
            state: state.into(),
            description: desc.map(|d| d.into()),
        }
    }

    #[test]
    fn build_audit_prompt_carries_identity_and_marks_empty_body() {
        let p = build_audit_prompt(&audit_input("Fix login", "Active", None), "");
        assert!(p.contains("- Type: Bug"));
        assert!(p.contains("- State: Active"));
        assert!(p.contains("- Title: Fix login"));
        assert!(p.contains("- Description: (empty)"));
        // Background block omitted when blank.
        assert!(!p.contains("TEAM BACKGROUND"));
    }

    #[test]
    fn build_audit_prompt_includes_background_and_truncates_body() {
        let body = "Q".repeat(MAX_DESC_CHARS + 200);
        let p = build_audit_prompt(
            &audit_input("t", "Active", Some(&body)),
            "Widget = our billing service.",
        );
        assert!(p.contains("TEAM BACKGROUND"));
        assert!(p.contains("Widget = our billing service."));
        assert_eq!(p.matches('Q').count(), MAX_DESC_CHARS);
    }

    #[test]
    fn parse_audit_keeps_known_kinds_dedupes_and_drops_blanks() {
        let raw = r#"Sure:
        ```json
        {"issues":[
          {"kind":"unclear","detail":"Title says 'fix it' without saying what."},
          {"kind":"UNCLEAR","detail":"Title says 'fix it' without saying what."},
          {"kind":"bad_data","detail":"  "},
          {"kind":"made_up","detail":"not a real kind"},
          {"kind":"bad_title","detail":"Placeholder title 'asdf'."}
        ]}
        ```"#;
        let issues = parse_audit_response(raw);
        // dedup drops the case-variant duplicate; blank detail + unknown kind dropped.
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].kind, AuditKind::Unclear);
        assert_eq!(issues[1].kind, AuditKind::BadTitle);
    }

    #[test]
    fn parse_audit_clean_or_garbage_yields_nothing() {
        assert!(parse_audit_response(r#"{"issues":[]}"#).is_empty());
        assert!(parse_audit_response("no json at all").is_empty());
        assert!(parse_audit_response("").is_empty());
    }

    #[test]
    fn parse_audit_caps_a_runaway_model() {
        let items = (0..MAX_AUDIT_ISSUES + 5)
            .map(|i| format!(r#"{{"kind":"unclear","detail":"concern {i}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!("{{\"issues\":[{items}]}}");
        assert_eq!(parse_audit_response(&raw).len(), MAX_AUDIT_ISSUES);
    }

    #[test]
    fn audit_kind_slug_round_trips() {
        for k in [AuditKind::Unclear, AuditKind::BadTitle, AuditKind::BadData] {
            assert_eq!(AuditKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        assert_eq!(AuditKind::parse("VAGUE"), Some(AuditKind::Unclear));
        assert_eq!(AuditKind::parse("nonsense-kind"), None);
    }

    // ── whole-item consistency sweep ─────────────────────────────────────────

    fn consistency_ctx() -> FieldsConsistencyContext {
        FieldsConsistencyContext {
            work_item_type: "Bug".into(),
            title: "Cost widgets zero out".into(),
            background: "Widget = our billing service.".into(),
            fields: vec![
                DraftFieldValue {
                    reference: "Microsoft.VSTS.TCM.ReproSteps".into(),
                    label: "Repro Steps".into(),
                    value: "Charts drop to zero.".into(),
                },
                DraftFieldValue {
                    reference: "Custom.Expected".into(),
                    label: "Expected behaviour".into(),
                    value: String::new(),
                },
            ],
            acceptance_criteria_gwt: true,
        }
    }

    #[test]
    fn build_consistency_prompt_lists_every_field_with_its_reference() {
        let p = build_fields_consistency_prompt(&consistency_ctx());
        assert!(p.contains("TEAM BACKGROUND"));
        assert!(p.contains("Type: Bug"));
        assert!(p.contains("reference: Microsoft.VSTS.TCM.ReproSteps"));
        assert!(p.contains("Charts drop to zero."));
        assert!(p.contains("reference: Custom.Expected"));
        assert!(p.contains("(empty)")); // the blank field is still listed
    }

    #[test]
    fn parse_consistency_keeps_known_refs_dropping_unknown_and_empty() {
        let refs = vec![
            "Microsoft.VSTS.TCM.ReproSteps".to_string(),
            "Custom.Expected".to_string(),
        ];
        let raw = r#"```json
        {"fields":[
          {"reference":"microsoft.vsts.tcm.reprosteps","value":"1. Open widget\n2. Observe zero"},
          {"reference":"Custom.Expected","value":"   "},
          {"reference":"System.Bogus","value":"not requested"}
        ]}
        ```"#;
        let out = parse_fields_consistency(raw, &refs);
        // case-insensitive ref match returns canonical spelling; empty + unknown dropped.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "Microsoft.VSTS.TCM.ReproSteps");
        assert!(out[0].1.contains("Open widget"));
    }

    #[test]
    fn strip_code_fence_unwraps_a_whole_answer_fence() {
        assert_eq!(
            strip_code_fence("```markdown\n- one\n- two\n```"),
            "- one\n- two"
        );
        assert_eq!(strip_code_fence("```\nplain\n```"), "plain");
        // Inline/partial fences are left alone (only an OUTER wrap is stripped).
        assert_eq!(strip_code_fence("text `code` more"), "text `code` more");
    }

    fn integ(id: &str, kind: &str) -> LlmIntegration {
        LlmIntegration {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    #[test]
    fn caps_suggestions_even_when_the_model_over_tags() {
        // The internal backstop only bites on a runaway model dumping far more than any
        // real per-item count; the real ceiling is applied downstream from config.
        let allowed: Vec<String> = (0..MAX_SUGGESTIONS + 5).map(|i| format!("t{i}")).collect();
        let tags = (0..MAX_SUGGESTIONS + 5)
            .map(|i| format!("\"t{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!("{{\"tags\":[{tags}],\"rationale\":\"dumped everything\"}}");
        assert_eq!(parse_suggestions(&raw, &allowed).len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn default_max_suggestions_scales_with_required_and_has_a_floor() {
        assert_eq!(default_max_suggestions(3), 10); // 3 required axes + slack
        assert_eq!(default_max_suggestions(0), 7); // no-required team
    }

    #[test]
    fn resolve_picks_first_platform_compatible_in_priority_order() {
        let mut gpu = integ("gpu", "offline");
        gpu.device = "gpu".into();
        gpu.offline_model = Some("qwen3-0.6b".into());
        let mut cloud = integ("cloud", "online");
        cloud.provider = Some("anthropic".into());
        cloud.api_key = Some("k".into()); // a cloud preset needs a key to be configured
        let cfg = LlmConfig {
            integrations: vec![gpu, cloud],
            ..Default::default()
        };

        // Embedded but no GPU: the gpu-offline entry is incompatible -> cloud wins.
        let cpu = PlatformCaps {
            embedded: true,
            gpu: false,
            webgpu: false,
            ..Default::default()
        };
        assert_eq!(cfg.active_id(&cpu), Some("cloud"));
        // GPU present: the higher-priority gpu-offline entry wins.
        let gpu_caps = PlatformCaps {
            embedded: true,
            gpu: true,
            webgpu: false,
            ..Default::default()
        };
        assert_eq!(cfg.active_id(&gpu_caps), Some("gpu"));
        // Browser client (no in-process embedded): only the online entry works.
        let browser = PlatformCaps {
            embedded: false,
            gpu: false,
            webgpu: false,
            ..Default::default()
        };
        assert_eq!(cfg.active_id(&browser), Some("cloud"));
    }

    #[test]
    fn seeded_catalog_grays_gpu_and_webgpu_on_a_cpu_server_and_activates_local_cpu() {
        let cfg = LlmConfig::seeded();
        // A plain CPU server: embedded yes, no CUDA, no WebGPU.
        let caps = PlatformCaps {
            embedded: true,
            gpu: false,
            webgpu: false,
            ..Default::default()
        };
        let by = |id: &str| cfg.integrations.iter().find(|i| i.id == id).unwrap();
        assert!(
            !by("local-gpu").compatible(&caps),
            "GPU entry unusable without CUDA"
        );
        assert!(
            !by("webgpu").compatible(&caps),
            "WebGPU entry unusable server-side"
        );
        assert!(by("local-cpu").compatible(&caps) && by("local-cpu").configured());
        // Local-first: the CPU embedded entry wins over the cloud/ollama entries.
        assert_eq!(cfg.active_id(&caps), Some("local-cpu"));
        // Keyless cloud presets are templates, not active backends.
        assert!(!by("claude").configured() && !by("openai").configured());
        // The custom Ollama endpoint needs no key, so it is configured (just lower priority).
        assert!(by("ollama").configured());
    }

    #[test]
    fn recommend_model_scales_with_capability() {
        let none = PlatformCaps::default();
        // WebGPU with no VRAM number -> balanced default (the load ladder tunes from there).
        assert_eq!(recommend_model("webgpu", "", &none), "qwen3-1.7b");
        // WebGPU sizes by ALLOCATABLE footprint from the catalog: the AUTO ceiling is 4B
        // (8B is hand-pick only, auto:false), so a big GPU lands on 4B - the quality/speed
        // sweet spot - and a weak GPU never pulls a model it can't fit.
        let big = PlatformCaps {
            vram_mb: Some(16000),
            ..Default::default()
        };
        assert_eq!(recommend_model("webgpu", "", &big), "qwen3-4b");
        let mid = PlatformCaps {
            vram_mb: Some(8000),
            ..Default::default()
        };
        assert_eq!(recommend_model("webgpu", "", &mid), "qwen3-4b");
        let small = PlatformCaps {
            vram_mb: Some(2000),
            ..Default::default()
        };
        assert_eq!(recommend_model("webgpu", "", &small), "qwen3-1.7b");
        let tiny = PlatformCaps {
            vram_mb: Some(1000),
            ..Default::default()
        };
        assert_eq!(recommend_model("webgpu", "", &tiny), "qwen3-0.6b");
        // CPU-only scales by cores.
        let many = PlatformCaps {
            cpu_cores: Some(16),
            ..Default::default()
        };
        let few = PlatformCaps {
            cpu_cores: Some(2),
            ..Default::default()
        };
        assert_eq!(recommend_model("offline", "cpu", &many), "qwen3-1.7b");
        assert_eq!(recommend_model("offline", "cpu", &few), "qwen3-0.6b");
    }

    #[test]
    fn seeded_for_sizes_local_models_and_marks_auto() {
        // A beefy WebGPU box: the webgpu entry gets bumped to the 4B auto ceiling; ordering
        // unchanged.
        let caps = PlatformCaps {
            webgpu: true,
            vram_mb: Some(16000),
            cpu_cores: Some(16),
            ..Default::default()
        };
        let cfg = LlmConfig::seeded_for(&caps);
        assert!(cfg.auto, "auto-configured registries are flagged");
        let by = |id: &str| cfg.integrations.iter().find(|i| i.id == id).unwrap();
        assert_eq!(by("webgpu").offline_model.as_deref(), Some("qwen3-4b"));
        assert_eq!(by("local-cpu").offline_model.as_deref(), Some("qwen3-1.7b"));
        // Same integrations, same order as the plain catalog.
        let plain = LlmConfig::seeded();
        assert_eq!(
            cfg.integrations.iter().map(|i| &i.id).collect::<Vec<_>>(),
            plain.integrations.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn normalize_models_heals_dead_ids_and_leaves_valid_ones() {
        let mut dead = integ("webgpu", "webgpu");
        dead.offline_model = Some("qwen2.5-7b".into()); // a pre-swap id, gone from the catalog
        let mut keep = integ("gpu", "offline");
        keep.device = "gpu".into();
        keep.offline_model = Some("qwen3-4b".into()); // still valid - the user's real pick
        let cloud = integ("claude", "online");
        let mut cfg = LlmConfig {
            integrations: vec![dead, keep, cloud],
            auto: false,
        };
        let caps = PlatformCaps::server();
        assert!(cfg.normalize_models(&caps), "a dead id is a change");
        let by = |id: &str| cfg.integrations.iter().find(|i| i.id == id).unwrap();
        // The dead id became a real catalog id...
        assert!(presets::offline_model(by("webgpu").offline_model.as_deref().unwrap()).is_some());
        // ...the valid id was left exactly as the user chose it...
        assert_eq!(by("gpu").offline_model.as_deref(), Some("qwen3-4b"));
        // ...and a second pass is a no-op (healed once, stays healed).
        assert!(!cfg.normalize_models(&caps));
    }

    #[test]
    fn demo_llm_config_offline_ids_match_the_catalog() {
        // The static landing-page demo fixture hand-mirrors the catalog. Pin its offline
        // model ids to OFFLINE_MODELS so a model-family swap can't silently strand it -
        // exactly what bit us on the Qwen2.5 → Qwen3 change. If this fails, regenerate
        // frontend/web/assets/demo/llm-config.json from the catalog.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../frontend/web/assets/demo/llm-config.json"
        );
        let Ok(txt) = std::fs::read_to_string(path) else {
            return; // fixture not present (crate built in isolation) - skip
        };
        let v: serde_json::Value = serde_json::from_str(&txt).expect("demo fixture is valid JSON");
        let mut demo_ids: Vec<String> = v["presets"]["offline"]
            .as_array()
            .expect("offline presets is an array")
            .iter()
            .map(|m| m["id"].as_str().expect("preset id").to_string())
            .collect();
        demo_ids.sort();
        let mut catalog_ids: Vec<String> =
            OFFLINE_MODELS.iter().map(|m| m.id.to_string()).collect();
        catalog_ids.sort();
        assert_eq!(
            demo_ids, catalog_ids,
            "demo/llm-config.json offline ids drifted from OFFLINE_MODELS - regenerate the fixture"
        );
    }

    #[test]
    fn webgpu_is_only_compatible_on_a_webgpu_platform() {
        let mut wg = integ("wg", "webgpu");
        wg.model = Some("qwen3-0.6b-q4f16".into());
        assert!(!wg.compatible(&PlatformCaps {
            embedded: true,
            gpu: true,
            webgpu: false,
            ..Default::default()
        }));
        assert!(wg.compatible(&PlatformCaps {
            embedded: false,
            gpu: false,
            webgpu: true,
            ..Default::default()
        }));
        // The server can't build a webgpu tagger (the browser runs it).
        assert!(wg.build().is_none());
    }

    #[test]
    fn keeps_only_allowed_tags_and_drops_hallucinations() {
        let raw = r#"{"tags": ["type:bug", "totally-made-up"], "rationale": "crash report"}"#;
        let s = parse_suggestions(raw, &allowed());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tag, "type:bug");
        assert_eq!(s[0].reasons, vec!["crash report".to_string()]);
    }

    #[test]
    fn matches_case_insensitively_but_returns_canonical_spelling() {
        let raw = r#"{"tags": ["TYPE:Bug", "Priority:HIGH"]}"#;
        let s: Vec<_> = parse_suggestions(raw, &allowed())
            .into_iter()
            .map(|x| x.tag)
            .collect();
        assert_eq!(s, vec!["type:bug".to_string(), "priority:high".to_string()]);
    }

    #[test]
    fn extracts_json_from_prose_and_code_fences() {
        let raw = "Sure! Here you go:\n```json\n{\"tags\": [\"type:task\"]}\n```\nHope that helps.";
        let s = parse_suggestions(raw, &allowed());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tag, "type:task");
    }

    #[test]
    fn de_duplicates_repeated_suggestions() {
        let raw = r#"{"tags": ["type:bug", "type:bug", "TYPE:BUG"]}"#;
        assert_eq!(parse_suggestions(raw, &allowed()).len(), 1);
    }

    #[test]
    fn garbage_or_empty_yields_nothing() {
        assert!(parse_suggestions("no json here", &allowed()).is_empty());
        assert!(parse_suggestions("", &allowed()).is_empty());
        assert!(parse_suggestions(r#"{"tags": []}"#, &allowed()).is_empty());
    }

    #[test]
    fn falls_back_to_generic_reason_when_rationale_absent() {
        let s = parse_suggestions(r#"{"tags": ["type:task"]}"#, &allowed());
        assert_eq!(s[0].reasons, vec!["AI suggestion".to_string()]);
    }

    #[test]
    fn config_off_by_default_and_online_builds_from_preset_or_custom() {
        assert!(!AiConfig::default().enabled() && AiConfig::default().build().is_none());
        // Online custom: needs endpoint + model.
        let custom = AiConfig {
            mode: "online".into(),
            provider: Some("custom".into()),
            endpoint: Some("http://x/v1/chat/completions".into()),
            model: Some("m".into()),
            ..Default::default()
        };
        assert!(custom.enabled() && custom.build().is_some());
        // Online preset: endpoint comes from the preset, model defaults.
        let preset = AiConfig {
            mode: "online".into(),
            provider: Some("anthropic".into()),
            api_key: Some("k".into()),
            ..Default::default()
        };
        assert!(preset.enabled() && preset.build().is_some());
        // Unknown provider / bare offline (no embedded engine yet) -> off.
        assert!(!AiConfig {
            mode: "online".into(),
            provider: Some("nope".into()),
            ..Default::default()
        }
        .enabled());
    }

    // ── parse_suggestions: the explicit "nothing applies" contract ───────────

    #[test]
    fn none_apply_reply_yields_no_suggestions() {
        // The system prompt tells the model to answer this exact shape when no tag
        // fits - it must parse to an empty suggestion list (the rationale is dropped
        // with the tags).
        let raw = r#"{"tags":[],"rationale":"none apply"}"#;
        assert!(parse_suggestions(raw, &allowed()).is_empty());
    }

    #[test]
    fn whitespace_around_tags_is_trimmed_before_matching() {
        // A model that pads tags with spaces still matches the allowed set.
        let raw = r#"{"tags":["  type:bug  ","\ttype:task\n"]}"#;
        let s: Vec<_> = parse_suggestions(raw, &allowed())
            .into_iter()
            .map(|x| x.tag)
            .collect();
        assert_eq!(s, vec!["type:bug".to_string(), "type:task".to_string()]);
    }

    // ── TaggerInput <- WorkItem ──────────────────────────────────────────────

    fn work_item(title: &str, ty: &str, tags: &[&str], desc: Option<&str>) -> WorkItem {
        WorkItem {
            id: 42,
            provider: "stub".into(),
            team: "Platform".into(),
            title: title.into(),
            work_item_type: ty.into(),
            state: "Active".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            assigned_to: None,
            created_at: Default::default(),
            changed_at: Default::default(),
            closed_at: None,
            iteration_path: None,
            story_points: None,
            description: desc.map(|d| d.to_string()),
            url: "https://x/42".into(),
            linked_pr_ids: Vec::new(),
            parent_id: None,
            linked_repos: Vec::new(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
        }
    }

    #[test]
    fn tagger_input_from_work_item_maps_the_minimal_fields() {
        let wi = work_item("Fix the poller", "Bug", &["type:bug"], Some("a body"));
        let input = TaggerInput::from(&wi);
        assert_eq!(input.id, 42);
        assert_eq!(input.title, "Fix the poller");
        assert_eq!(input.work_item_type, "Bug");
        assert_eq!(input.current_tags, vec!["type:bug".to_string()]);
        assert_eq!(input.description.as_deref(), Some("a body"));
    }

    #[test]
    fn tagger_input_carries_absent_description_through() {
        let input = TaggerInput::from(&work_item("t", "Task", &[], None));
        assert!(input.description.is_none());
        assert!(input.current_tags.is_empty());
    }

    // ── build_prompt ─────────────────────────────────────────────────────────

    #[test]
    fn build_prompt_includes_title_type_and_allowed_set() {
        let input = TaggerInput::from(&work_item(
            "Wire up ingress",
            "User Story",
            &["team:platform"],
            None,
        ));
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default(), "");
        assert!(prompt.contains("- Title: Wire up ingress"));
        assert!(prompt.contains("- Type: User Story"));
        assert!(prompt.contains("- Current tags: team:platform"));
        // Allowed tags are rendered one-per-line, bulleted.
        assert!(prompt.contains("- type:bug"));
        assert!(prompt.contains("- priority:high"));
    }

    #[test]
    fn build_prompt_uses_none_placeholder_for_untagged() {
        let input = TaggerInput::from(&work_item("t", "Task", &[], None));
        assert!(
            build_prompt(&input, &allowed(), &[], &Default::default(), "")
                .contains("- Current tags: (none)")
        );
    }

    #[test]
    fn build_prompt_omits_description_when_absent_or_blank() {
        let none = TaggerInput::from(&work_item("t", "Task", &[], None));
        assert!(
            !build_prompt(&none, &allowed(), &[], &Default::default(), "").contains("Description:")
        );
        // A whitespace-only body is treated as absent (trimmed, then filtered out).
        let blank = TaggerInput::from(&work_item("t", "Task", &[], Some("   \n\t ")));
        assert!(
            !build_prompt(&blank, &allowed(), &[], &Default::default(), "")
                .contains("Description:")
        );
    }

    #[test]
    fn build_prompt_includes_description_when_present() {
        let input = TaggerInput::from(&work_item("t", "Task", &[], Some("crashes on retry")));
        assert!(
            build_prompt(&input, &allowed(), &[], &Default::default(), "")
                .contains("- Description: crashes on retry")
        );
    }

    #[test]
    fn build_prompt_truncates_a_long_description_to_the_cap() {
        // A body over the cap is cut to MAX_DESC_CHARS chars. Use a marker char the
        // title/type/tags never contain so the count is unambiguous.
        let body = "Z".repeat(MAX_DESC_CHARS + 500);
        let input = TaggerInput::from(&work_item("t", "Task", &[], Some(&body)));
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default(), "");
        assert_eq!(prompt.matches('Z').count(), MAX_DESC_CHARS);
    }

    #[test]
    fn build_prompt_groups_unsatisfied_required_slots_with_their_options() {
        // area:* is already satisfied by a current tag -> not asked again.
        // source:* is unsatisfied -> a REQUIRED category listing only source values.
        let input = TaggerInput::from(&work_item(
            "Provision staging environment",
            "Feature",
            &["area:kube"],
            None,
        ));
        let allowed = vec![
            "area:kube".to_string(),
            "area:auth".to_string(),
            "source:support".to_string(),
            "source:incident".to_string(),
            "enhancement".to_string(),
        ];
        let required = vec!["area:*".to_string(), "source:*".to_string()];
        let prompt = build_prompt(&input, &allowed, &required, &Default::default(), "");

        assert!(
            prompt.contains("REQUIRED categories"),
            "has a required block"
        );
        assert!(prompt.contains("- source: (choose one): source:support, source:incident"));
        // area:* is satisfied, so it must NOT appear as a required category to fill.
        assert!(!prompt.contains("- area: (choose one)"));
        // The source values are claimed by the required block, so they are not
        // repeated in the optional list; the ungoverned tag still is.
        let optional = prompt.split("OPTIONAL tags").nth(1).unwrap_or("");
        assert!(optional.contains("- enhancement"));
        assert!(!optional.contains("source:support"));
    }

    #[test]
    fn build_prompt_annotates_candidates_with_their_keyword_hints() {
        let input = TaggerInput::from(&work_item(
            "Regional infrastructure rollout",
            "Epic",
            &[],
            None,
        ));
        let allowed = vec![
            "area:platform-deployment".to_string(),
            "area:kubernetes".to_string(),
        ];
        let required = vec!["area:*".to_string()];
        let mut hints = TagHints::new();
        hints.insert(
            "area:platform-deployment".into(),
            vec!["platform deployment".into(), "platformdeployment".into()],
        );
        let prompt = build_prompt(&input, &allowed, &required, &hints, "");
        // The hinted tag shows its keywords; the un-hinted one is bare.
        assert!(prompt
            .contains("area:platform-deployment (e.g. platform deployment, platformdeployment)"));
        assert!(prompt.contains("area:kubernetes"));
        assert!(!prompt.contains("area:kubernetes (e.g."));
    }

    #[test]
    fn build_prompt_caps_keyword_hints_per_tag() {
        let input = TaggerInput::from(&work_item("t", "Task", &[], None));
        let allowed = vec!["area:kubernetes".to_string()];
        let many: Vec<String> = (0..20).map(|i| format!("kw{i}")).collect();
        let mut hints = TagHints::new();
        hints.insert("area:kubernetes".into(), many);
        let prompt = build_prompt(&input, &allowed, &["area:*".to_string()], &hints, "");
        // Only MAX_HINTS_PER_TAG examples are shown.
        assert_eq!(prompt.matches("kw").count(), MAX_HINTS_PER_TAG);
    }

    #[test]
    fn build_prompt_without_required_matches_the_plain_allowed_layout() {
        // No required patterns -> the old "ALLOWED tags:" layout, no required block.
        let input = TaggerInput::from(&work_item("t", "Task", &[], None));
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default(), "");
        assert!(prompt.contains("ALLOWED tags:"));
        assert!(!prompt.contains("REQUIRED categories"));
    }
}
