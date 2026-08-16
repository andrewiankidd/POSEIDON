//! POSEIDEN's hygiene rules engine.
//!
//! One entry point: [`evaluate`] takes the work items from a poll plus the
//! configured [`RuleSet`] and returns the [`Flag`]s. It is a pure function of
//! its inputs (the only ambient value, "now", is passed in) - which makes it
//! trivially testable and keeps *policy* in config while *mechanism* stays
//! here in code. Rules are data; this crate is the interpreter.
//!
//! Checks performed per item (in order), skipping items whose state or type is
//! on the ruleset's ignore lists:
//!
//! 1. **Untagged** - no tags at all.
//! 2. **Missing required tag** - a `required_tags` pattern matches nothing.
//! 3. **Disallowed tag** - a tag hits `disallowed_tags`, or (when an allow-list
//!    is configured) fails to match any `allowed_tags` pattern.
//! 4. **Stale** - item has sat in its state past that state's day limit.

use chrono::{DateTime, Utc};
use poseiden_core::{
    EntityFlag, Flag, FlagCode, PipelineRules, PrRules, PrStatus, PullRequest, RuleSet, RunStatus,
    Severity, TagSuggestion, WorkItem,
};

/// Whether `needle` occurs in `haystack` as a WHOLE token, not an arbitrary
/// substring - i.e. the characters immediately before and after the match are not
/// ASCII alphanumeric. This stops short keywords from firing on unrelated words
/// (e.g. `ado` in "adopted", `imm` in "immediate", `cas` in "cascade"). Both are
/// assumed already lower-cased. Multi-word keywords ("azure devops") are matched as
/// a whole, boundary-checked at the outer edges. Byte-based boundary checks (word
/// chars are ASCII) avoid UTF-8 slicing panics on non-ASCII bodies.
pub fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hb = haystack.as_bytes();
    let nl = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let i = start + pos;
        let before_ok = i == 0 || !hb[i - 1].is_ascii_alphanumeric();
        let after = i + nl;
        let after_ok = after >= hb.len() || !hb[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
    }
    false
}

/// Default underspecified threshold when `refine_tag` is set but no explicit
/// `refine_min_chars` is given.
pub const DEFAULT_REFINE_MIN_CHARS: usize = 40;

/// True when the item has too little descriptive body to tag from - its description
/// (trimmed) is shorter than the configured/default threshold. Only meaningful when
/// `refine_tag` is configured (else always false). The TITLE is ignored on purpose:
/// a title is always present; it's the BODY that carries the signal a model (or a
/// person) needs to assign a real area/source. Callers use this to suggest the
/// refine tag and to SKIP the AI tagger (so it can't guess an area from nothing).
pub fn is_underspecified(item: &WorkItem, rules: &RuleSet) -> bool {
    let has_refine = rules
        .refine_tag
        .as_deref()
        .map(str::trim)
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    if !has_refine {
        return false;
    }
    // Measure the MEANINGFUL body: a description that is mostly a hyperlink to an
    // external doc (a SharePoint/Loop URL) isn't real detail, so links are stripped
    // before counting - otherwise a one-line "see [link]" stub reads as substantial.
    let body = item
        .description
        .as_deref()
        .map(meaningful_body)
        .unwrap_or_default();
    // Placeholder stubs: real words, no actionable content ("scope to be clarified",
    // "TBD"). Length can't catch these, so match configured phrases (case-insensitive
    // substring). Empty list = off; deterministic, no model needed.
    let low = body.to_lowercase();
    if rules
        .refine_phrases
        .iter()
        .map(|p| p.trim())
        .any(|p| !p.is_empty() && low.contains(&p.to_lowercase()))
    {
        return true;
    }
    let min = rules.refine_min_chars.unwrap_or(DEFAULT_REFINE_MIN_CHARS);
    body.chars().count() < min
}

/// The descriptive body with hyperlinks removed, so a body that is mostly a link to an
/// external doc doesn't read as substantial. Keeps ordinary words; drops URL tokens
/// (anything with `://` or a leading `www.`) and surrounding markdown punctuation;
/// collapses whitespace. Used by [`is_underspecified`] so the refine gate measures real
/// prose, not the length of a pasted SharePoint URL.
fn meaningful_body(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    for tok in desc.split_whitespace() {
        let t = tok.trim_matches(|c: char| "()[]<>\"'`*|".contains(c));
        if t.is_empty() || t.contains("://") || t.starts_with("www.") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out.trim().to_string()
}

/// Suggest tags for a work item from the ruleset's `tag_keywords`: for each tag
/// whose keyword the item matches (case-insensitive) and which the item doesn't
/// already carry, return the tag with the matched keyword(s) as the reason. Matches
/// the title always, plus the description when `use_description` (owner opt-in). Also
/// suggests the `refine_tag` for underspecified items (see [`is_underspecified`]).
/// Advisory only - the caller decides whether to apply. Pure + testable.
pub fn suggest_tags(item: &WorkItem, rules: &RuleSet, use_description: bool) -> Vec<TagSuggestion> {
    let mut haystack = item.title.to_lowercase();
    if use_description {
        if let Some(desc) = item.description.as_deref() {
            haystack.push('\n');
            haystack.push_str(&desc.to_lowercase());
        }
    }
    let has: std::collections::HashSet<String> =
        item.tags.iter().map(|t| t.to_lowercase()).collect();
    let mut out: Vec<TagSuggestion> = rules
        .tag_keywords
        .iter()
        .filter(|tk| !tk.tag.trim().is_empty() && !has.contains(&tk.tag.to_lowercase()))
        .filter_map(|tk| {
            let reasons: Vec<String> = tk
                .keywords
                .iter()
                .map(|k| k.trim())
                .filter(|k| !k.is_empty() && contains_word(&haystack, &k.to_lowercase()))
                .map(str::to_string)
                .collect();
            (!reasons.is_empty()).then(|| TagSuggestion {
                tag: tk.tag.clone(),
                reasons,
                replaces: None,
            })
        })
        .collect();

    // Tag alias / rewrite rules: if the item carries a LEGACY tag matching `from`,
    // suggest the canonical `to` as a REWRITE (apply = add `to` + drop the matched
    // `from`). Deterministic - no AI, no title/body needed - which is the whole point:
    // it migrates `SSA` -> `area:ssa` for users who can't (or won't) run a model.
    let mut suggested: std::collections::HashSet<String> =
        out.iter().map(|s| s.tag.to_lowercase()).collect();
    for alias in &rules.tag_aliases {
        let (from, to) = (alias.from.trim(), alias.to.trim());
        if from.is_empty() || to.is_empty() || has.contains(&to.to_lowercase()) {
            continue;
        }
        if let Some(matched) = item.tags.iter().find(|t| tag_matches(from, t)) {
            if suggested.insert(to.to_lowercase()) {
                out.push(TagSuggestion {
                    tag: to.to_string(),
                    reasons: vec![format!("replaces \"{matched}\"")],
                    replaces: Some(matched.clone()),
                });
            }
        }
    }

    // Underspecified items: too little body to tag from. Rather than let the AI
    // guess an area from nothing, suggest the refine tag so the item is flagged for
    // refinement first - but only when it still NEEDS a required tag and isn't
    // already carrying the refine tag. Deterministic; no model needed.
    //
    // NEVER on a DONE item (resolved/closed/ignored state): a "needs refinement" tag on
    // finished work is contradictory - the stale-when-resolved rule would immediately
    // flag it for removal - so refining a terminal item is nonsensical (else the tool
    // contradicts itself: it suggests a tag it would then flag).
    let terminal_state = is_ignored(item, rules)
        || rules
            .resolved_states
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&item.state));
    if !terminal_state && is_underspecified(item, rules) {
        if let Some(tag) = rules
            .refine_tag
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let missing_required = rules.required_tags.iter().any(|p| {
                let p = p.trim();
                !p.is_empty() && !item.tags.iter().any(|t| tag_matches(p, t))
            });
            if missing_required
                && !has.contains(&tag.to_lowercase())
                && suggested.insert(tag.to_lowercase())
            {
                out.push(TagSuggestion {
                    tag: tag.to_string(),
                    reasons: vec![
                        "underspecified - too little detail to tag; refine first".to_string()
                    ],
                    replaces: None,
                });
            }
        }
    }

    // Final guard: on a DONE item (resolved/closed/ignored state) never suggest a tag
    // that the stale-when-resolved rule would immediately flag as contradictory - "to
    // refine", "to do", "in progress", "blocked", … on finished work. This catches EVERY
    // source (keyword match, an alias rewrite like legacy "Refine" -> "to refine", or the
    // refine nudge above): suggesting "needs more work" on completed work is always wrong.
    let terminal = is_ignored(item, rules)
        || rules
            .resolved_states
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&item.state));
    if terminal && !rules.stale_when_resolved_tags.is_empty() {
        out.retain(|s| {
            !rules
                .stale_when_resolved_tags
                .iter()
                .map(|p| p.trim())
                .any(|p| !p.is_empty() && tag_matches(p, &s.tag))
        });
    }
    out
}

/// Evaluate a pipeline's hygiene flags from its most-recent-run status and the
/// team's [`PipelineRules`]. Pure - same inputs, same flags.
pub fn evaluate_pipeline(last_status: Option<RunStatus>, rules: &PipelineRules) -> Vec<EntityFlag> {
    let mut flags = Vec::new();
    if rules.flag_never_run && last_status.is_none() {
        flags.push(EntityFlag {
            code: "never-run".into(),
            severity: Severity::Warn,
            message: "pipeline has never run".into(),
        });
    }
    if rules.flag_failing && last_status == Some(RunStatus::Failed) {
        flags.push(EntityFlag {
            code: "failing".into(),
            severity: Severity::Error,
            message: "most recent run failed".into(),
        });
    }
    flags
}

/// Evaluate a pull request's hygiene flags against the team's [`PrRules`]. Only
/// active PRs are checked; a `None`/non-positive threshold disables that check.
pub fn evaluate_pull_request(
    pr: &PullRequest,
    rules: &PrRules,
    now: DateTime<Utc>,
) -> Vec<EntityFlag> {
    let mut flags = Vec::new();
    if pr.status != PrStatus::Active {
        return flags;
    }
    // Traceability: an active PR should link to a work item.
    if rules.require_work_item && pr.linked_work_items.is_empty() {
        flags.push(EntityFlag {
            code: "no-work-item".into(),
            severity: Severity::Warn,
            message: "no linked work item".into(),
        });
    }
    let age_days = pr.created_at.map(|c| (now - c).num_days().max(0));
    let (limit, code, label) = if pr.is_draft {
        (rules.stale_draft_days, "stale-draft", "draft open")
    } else {
        (rules.stale_open_days, "stale-open", "open")
    };
    if let (Some(limit), Some(age)) = (limit.filter(|d| *d > 0), age_days) {
        if age > limit {
            flags.push(EntityFlag {
                code: code.into(),
                severity: Severity::Warn,
                message: format!("{label} {age} days (limit {limit})"),
            });
        }
    }
    flags
}

/// Match a single tag against a pattern. A trailing `*` is a prefix wildcard
/// (`"type:*"` matches `"type:bug"`); otherwise it's an exact match. Both sides
/// are compared case-insensitively, matching provider tag semantics.
pub fn tag_matches(pattern: &str, tag: &str) -> bool {
    let pattern = pattern.trim();
    let tag = tag.trim();
    if let Some(prefix) = pattern.strip_suffix('*') {
        tag.to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    } else {
        pattern.eq_ignore_ascii_case(tag)
    }
}

/// Does any tag on the item satisfy `pattern`?
fn any_tag_matches(pattern: &str, item: &WorkItem) -> bool {
    item.tags.iter().any(|t| tag_matches(pattern, t))
}

/// Evaluate every item against the ruleset. `now` anchors staleness (injected
/// rather than read from the clock so results are deterministic + testable).
/// Flags come back grouped per item in input order, then per-check in the
/// order listed above.
pub fn evaluate(items: &[WorkItem], rules: &RuleSet, now: DateTime<Utc>) -> Vec<Flag> {
    let mut flags = Vec::new();
    for item in items {
        // Stale-state tags run even on ignore_states: a terminal state is exactly
        // what ignore_states exempts, yet a leftover "still needs work" tag there is
        // the contradiction we specifically want to surface.
        evaluate_resolved_stale_tags(item, &mut flags, rules);
        if is_ignored(item, rules) {
            continue;
        }
        evaluate_item(item, rules, now, &mut flags);
    }
    flags
}

/// Flag a tag that implies outstanding work on an item that is already resolved
/// (e.g. "to refine" on a Closed story). Pure content classification (the AI tagger)
/// can't see this - it's a state↔tag contradiction, so it lives here in the rules.
fn evaluate_resolved_stale_tags(item: &WorkItem, flags: &mut Vec<Flag>, rules: &RuleSet) {
    if rules.resolved_states.is_empty() || rules.stale_when_resolved_tags.is_empty() {
        return;
    }
    let resolved = rules
        .resolved_states
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&item.state));
    if !resolved {
        return;
    }
    for tag in &item.tags {
        if rules
            .stale_when_resolved_tags
            .iter()
            .any(|p| tag_matches(p, tag))
        {
            flags.push(Flag {
                work_item_id: item.id,
                team: item.team.clone(),
                code: FlagCode::StaleStateTag,
                severity: Severity::Warn,
                message: format!(
                    "tag \"{tag}\" implies open work, but the item is resolved (state \"{}\")",
                    item.state
                ),
                tag: Some(tag.clone()),
            });
        }
    }
}

/// Whether an item is exempt from all checks (state or type on an ignore list).
fn is_ignored(item: &WorkItem, rules: &RuleSet) -> bool {
    rules
        .ignore_states
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&item.state))
        || rules
            .ignore_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&item.work_item_type))
}

fn evaluate_item(item: &WorkItem, rules: &RuleSet, now: DateTime<Utc>, flags: &mut Vec<Flag>) {
    let untagged_severity = if rules.untagged_is_error {
        Severity::Error
    } else {
        Severity::Warn
    };

    // 1. Untagged. When an item has no tags we emit the single "untagged" flag
    //    and skip the required/allowed tag checks - flagging every missing
    //    required tag on top would be noise saying the same thing.
    if item.is_untagged() {
        flags.push(Flag {
            work_item_id: item.id,
            team: item.team.clone(),
            code: FlagCode::Untagged,
            severity: untagged_severity,
            message: "item has no tags".to_string(),
            tag: None,
        });
    } else {
        // 2. Missing required tags.
        for pattern in &rules.required_tags {
            if !any_tag_matches(pattern, item) {
                flags.push(Flag {
                    work_item_id: item.id,
                    team: item.team.clone(),
                    code: FlagCode::MissingRequiredTag,
                    severity: Severity::Error,
                    message: format!("missing required tag matching \"{pattern}\""),
                    tag: Some(pattern.clone()),
                });
            }
        }

        // 3. Disallowed tags. Explicit deny-list first, then allow-list
        //    violations (only when an allow-list is configured).
        for tag in &item.tags {
            let denied = rules.disallowed_tags.iter().any(|p| tag_matches(p, tag));
            let not_allowed = !rules.allowed_tags.is_empty()
                && !rules.allowed_tags.iter().any(|p| tag_matches(p, tag));
            if denied || not_allowed {
                let reason = if denied {
                    "on the disallowed list"
                } else {
                    "not on the allowed list"
                };
                flags.push(Flag {
                    work_item_id: item.id,
                    team: item.team.clone(),
                    code: FlagCode::DisallowedTag,
                    severity: Severity::Warn,
                    message: format!("tag \"{tag}\" is {reason}"),
                    tag: Some(tag.clone()),
                });
            }
        }
    }

    // 4. Staleness - independent of tagging, so it runs for tagged and
    //    untagged items alike.
    for rule in rules.stale_rules() {
        if rule.state.eq_ignore_ascii_case(&item.state) {
            let age = item.days_since_change(now);
            if age > rule.days {
                flags.push(Flag {
                    work_item_id: item.id,
                    team: item.team.clone(),
                    code: FlagCode::Stale,
                    severity: Severity::Warn,
                    message: format!(
                        "stale: {age} days in \"{}\" (limit {})",
                        item.state, rule.days
                    ),
                    tag: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap()
    }

    /// Build a work item with sensible defaults; override what a test cares
    /// about via the closure.
    fn item(f: impl FnOnce(&mut WorkItem)) -> WorkItem {
        let mut wi = WorkItem {
            id: 1,
            provider: "azure-devops".to_string(),
            team: "Platform".to_string(),
            title: "Do the thing".to_string(),
            work_item_type: "User Story".to_string(),
            state: "Active".to_string(),
            tags: vec!["team:platform".to_string(), "type:feature".to_string()],
            assigned_to: Some("Ada".to_string()),
            created_at: now() - chrono::Duration::days(30),
            changed_at: now() - chrono::Duration::days(1),
            closed_at: None,
            iteration_path: None,
            story_points: None,
            url: "https://example/1".to_string(),
            description: None,
            linked_pr_ids: Vec::new(),
            parent_id: None,
            linked_repos: Vec::new(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
        };
        f(&mut wi);
        wi
    }

    #[test]
    fn flags_open_work_tags_on_resolved_items() {
        let rules = RuleSet {
            resolved_states: vec!["Closed".into(), "Done".into()],
            stale_when_resolved_tags: vec!["to refine".into(), "wip".into()],
            ..Default::default()
        };
        // A Closed item still carrying "To Refine" is contradictory -> flagged
        // (case-insensitive), with the offending tag attached for a "remove" action.
        let closed = item(|w| {
            w.state = "Closed".into();
            w.tags = vec!["type:feature".into(), "To Refine".into()];
        });
        let flags = evaluate(&[closed], &rules, now());
        let f = flags
            .iter()
            .find(|f| f.code == FlagCode::StaleStateTag)
            .expect("stale-state-tag flag");
        assert_eq!(f.tag.as_deref(), Some("To Refine"));
        assert_eq!(f.severity, Severity::Warn);

        // The same tag on an ACTIVE item is perfectly fine.
        let active = item(|w| w.tags = vec!["To Refine".into()]);
        assert!(evaluate(&[active], &rules, now())
            .iter()
            .all(|f| f.code != FlagCode::StaleStateTag));

        // Runs even when the terminal state is ALSO exempted via ignore_states -
        // that's the whole point (a leftover work tag on a "done and ignored" item).
        let ignored = RuleSet {
            ignore_states: vec!["Closed".into()],
            ..rules.clone()
        };
        let closed_wip = item(|w| {
            w.state = "Closed".into();
            w.tags = vec!["wip".into()];
        });
        assert!(evaluate(&[closed_wip], &ignored, now())
            .iter()
            .any(|f| f.code == FlagCode::StaleStateTag));
    }

    #[test]
    fn clean_item_produces_no_flags() {
        let rules = RuleSet {
            required_tags: vec!["team:*".into(), "type:*".into()],
            allowed_tags: vec!["team:*".into(), "type:*".into()],
            ..Default::default()
        };
        let flags = evaluate(&[item(|_| {})], &rules, now());
        assert!(
            flags.is_empty(),
            "clean item should not be flagged: {flags:?}"
        );
    }

    #[test]
    fn untagged_item_is_flagged_once_not_per_required_tag() {
        let rules = RuleSet {
            required_tags: vec!["team:*".into(), "type:*".into()],
            ..Default::default()
        };
        let flags = evaluate(&[item(|i| i.tags.clear())], &rules, now());
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].code, FlagCode::Untagged);
        assert_eq!(flags[0].severity, Severity::Warn);
    }

    #[test]
    fn untagged_severity_follows_config() {
        let rules = RuleSet {
            untagged_is_error: true,
            ..Default::default()
        };
        let flags = evaluate(&[item(|i| i.tags.clear())], &rules, now());
        assert_eq!(flags[0].severity, Severity::Error);
    }

    #[test]
    fn missing_required_tag_flagged_when_pattern_unmatched() {
        let rules = RuleSet {
            required_tags: vec!["team:*".into(), "priority:*".into()],
            ..Default::default()
        };
        // Item has team: + type: but no priority: tag.
        let flags = evaluate(&[item(|_| {})], &rules, now());
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].code, FlagCode::MissingRequiredTag);
        assert_eq!(flags[0].tag.as_deref(), Some("priority:*"));
    }

    #[test]
    fn disallowed_deny_list_tag_flagged() {
        let rules = RuleSet {
            disallowed_tags: vec!["wip".into()],
            ..Default::default()
        };
        let flags = evaluate(&[item(|i| i.tags.push("wip".into()))], &rules, now());
        let codes: Vec<_> = flags.iter().map(|f| f.code).collect();
        assert!(codes.contains(&FlagCode::DisallowedTag));
        let wip = flags
            .iter()
            .find(|f| f.code == FlagCode::DisallowedTag)
            .unwrap();
        assert_eq!(wip.tag.as_deref(), Some("wip"));
        assert!(wip.message.contains("disallowed"));
    }

    #[test]
    fn allow_list_violation_flagged_only_when_allow_list_set() {
        // With an allow-list that doesn't cover "random", the tag is flagged.
        let with_allow = RuleSet {
            allowed_tags: vec!["team:*".into(), "type:*".into()],
            ..Default::default()
        };
        let flags = evaluate(
            &[item(|i| i.tags.push("random".into()))],
            &with_allow,
            now(),
        );
        assert!(flags.iter().any(|f| f.code == FlagCode::DisallowedTag
            && f.tag.as_deref() == Some("random")
            && f.message.contains("not on the allowed list")));

        // With NO allow-list, the same tag is fine.
        let no_allow = RuleSet::default();
        let flags = evaluate(&[item(|i| i.tags.push("random".into()))], &no_allow, now());
        assert!(flags.is_empty());
    }

    #[test]
    fn stale_item_flagged_past_state_limit() {
        let mut stale_days = BTreeMap::new();
        stale_days.insert("Active".to_string(), 5);
        let rules = RuleSet {
            stale_days,
            ..Default::default()
        };
        // Changed 9 days ago, limit 5 → stale.
        let flags = evaluate(
            &[item(|i| i.changed_at = now() - chrono::Duration::days(9))],
            &rules,
            now(),
        );
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].code, FlagCode::Stale);
        assert!(flags[0].message.contains("9 days"));
    }

    #[test]
    fn fresh_item_within_limit_not_stale() {
        let mut stale_days = BTreeMap::new();
        stale_days.insert("Active".to_string(), 5);
        let rules = RuleSet {
            stale_days,
            ..Default::default()
        };
        // Changed 2 days ago, limit 5 → fine.
        let flags = evaluate(
            &[item(|i| i.changed_at = now() - chrono::Duration::days(2))],
            &rules,
            now(),
        );
        assert!(flags.is_empty());
    }

    #[test]
    fn ignored_state_exempts_item_from_all_checks() {
        let rules = RuleSet {
            required_tags: vec!["nonexistent:*".into()],
            ignore_states: vec!["Closed".into()],
            ..Default::default()
        };
        // Untagged + missing required tag, but Closed → skipped entirely.
        let flags = evaluate(
            &[item(|i| {
                i.state = "Closed".into();
                i.tags.clear();
            })],
            &rules,
            now(),
        );
        assert!(flags.is_empty());
    }

    #[test]
    fn ignored_type_exempts_item() {
        let rules = RuleSet {
            untagged_is_error: true,
            ignore_types: vec!["Task".into()],
            ..Default::default()
        };
        let flags = evaluate(
            &[item(|i| {
                i.work_item_type = "Task".into();
                i.tags.clear();
            })],
            &rules,
            now(),
        );
        assert!(flags.is_empty());
    }

    #[test]
    fn tag_matches_wildcard_and_exact_case_insensitive() {
        assert!(tag_matches("type:*", "type:bug"));
        assert!(tag_matches("type:*", "TYPE:Bug"));
        assert!(tag_matches("team:platform", "team:Platform"));
        assert!(!tag_matches("team:platform", "team:data"));
        assert!(!tag_matches("type:*", "priority:high"));
    }

    // ── Pull-request rules ──────────────────────────────────────────────

    /// Build an active PR with sensible defaults (linked to a work item, fresh,
    /// non-draft); override what a test cares about via the closure.
    fn pr(f: impl FnOnce(&mut PullRequest)) -> PullRequest {
        let mut p = PullRequest {
            id: 100,
            provider: "azure-devops".to_string(),
            team: "Platform".to_string(),
            title: "Add retry to poller".to_string(),
            status: PrStatus::Active,
            is_draft: false,
            repository: Some("platform-svc".to_string()),
            author: Some("Ada".to_string()),
            created_at: Some(now() - chrono::Duration::days(1)),
            source_branch: Some("feature/retry".to_string()),
            target_branch: Some("main".to_string()),
            reviewer_count: 1,
            url: "https://example/pr/100".to_string(),
            flags: Vec::new(),
            linked_work_items: vec![42],
        };
        f(&mut p);
        p
    }

    fn codes(flags: &[EntityFlag]) -> Vec<&str> {
        flags.iter().map(|f| f.code.as_str()).collect()
    }

    #[test]
    fn pr_clean_produces_no_flags() {
        let rules = PrRules {
            require_work_item: true,
            stale_open_days: Some(14),
            ..Default::default()
        };
        assert!(evaluate_pull_request(&pr(|_| {}), &rules, now()).is_empty());
    }

    #[test]
    fn pr_require_work_item_flags_only_when_unlinked() {
        let rules = PrRules {
            require_work_item: true,
            ..Default::default()
        };
        let missing = pr(|p| p.linked_work_items.clear());
        assert_eq!(
            codes(&evaluate_pull_request(&missing, &rules, now())),
            ["no-work-item"]
        );
        // Linked -> no flag.
        assert!(evaluate_pull_request(&pr(|_| {}), &rules, now()).is_empty());
    }

    #[test]
    fn pr_require_work_item_off_never_flags() {
        let rules = PrRules::default();
        let missing = pr(|p| p.linked_work_items.clear());
        assert!(evaluate_pull_request(&missing, &rules, now()).is_empty());
    }

    #[test]
    fn pr_non_active_is_exempt_from_all_checks() {
        // A completed PR with no work item + a stale age is still not flagged.
        let rules = PrRules {
            require_work_item: true,
            stale_open_days: Some(1),
            ..Default::default()
        };
        let completed = pr(|p| {
            p.status = PrStatus::Completed;
            p.linked_work_items.clear();
            p.created_at = Some(now() - chrono::Duration::days(90));
        });
        assert!(evaluate_pull_request(&completed, &rules, now()).is_empty());
    }

    #[test]
    fn pr_stale_open_uses_open_limit_and_draft_uses_draft_limit() {
        let rules = PrRules {
            stale_open_days: Some(14),
            stale_draft_days: Some(3),
            ..Default::default()
        };
        let old_open = pr(|p| p.created_at = Some(now() - chrono::Duration::days(20)));
        assert_eq!(
            codes(&evaluate_pull_request(&old_open, &rules, now())),
            ["stale-open"]
        );
        let old_draft = pr(|p| {
            p.is_draft = true;
            p.created_at = Some(now() - chrono::Duration::days(5));
        });
        assert_eq!(
            codes(&evaluate_pull_request(&old_draft, &rules, now())),
            ["stale-draft"]
        );
        // A draft 5 days old is NOT past the 14-day open limit, proving the draft
        // path uses its own (shorter) threshold.
        let fresh_open = pr(|p| p.created_at = Some(now() - chrono::Duration::days(5)));
        assert!(evaluate_pull_request(&fresh_open, &rules, now()).is_empty());
    }

    // ── Pipeline rules ──────────────────────────────────────────────────

    #[test]
    fn pipeline_never_run_and_failing_flag_when_enabled() {
        let rules = PipelineRules {
            flag_failing: true,
            flag_never_run: true,
        };
        assert_eq!(codes(&evaluate_pipeline(None, &rules)), ["never-run"]);
        assert_eq!(
            codes(&evaluate_pipeline(Some(RunStatus::Failed), &rules)),
            ["failing"]
        );
        // A healthy last run trips neither.
        assert!(evaluate_pipeline(Some(RunStatus::Succeeded), &rules).is_empty());
    }

    #[test]
    fn pipeline_flags_off_never_fire() {
        let rules = PipelineRules::default();
        assert!(evaluate_pipeline(None, &rules).is_empty());
        assert!(evaluate_pipeline(Some(RunStatus::Failed), &rules).is_empty());
    }

    // ── Tag auto-suggest ────────────────────────────────────────────────

    #[test]
    fn suggest_tags_from_title_keywords() {
        use poseiden_core::TagKeywords;
        let rules = RuleSet {
            tag_keywords: vec![
                TagKeywords {
                    tag: "type:bug".into(),
                    keywords: vec!["error".into(), "crash".into()],
                },
                TagKeywords {
                    tag: "Documentation".into(),
                    keywords: vec!["docs".into()],
                },
            ],
            ..Default::default()
        };
        // "Crash" matches type:bug (case-insensitive); docs is absent.
        let it = item(|w| {
            w.title = "Fix Crash on startup".into();
            w.tags = vec![];
        });
        let s = suggest_tags(&it, &rules, false);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tag, "type:bug");
        assert_eq!(s[0].reasons, vec!["crash"]);
    }

    #[test]
    fn suggest_tags_matches_description_only_when_opted_in() {
        use poseiden_core::TagKeywords;
        let rules = RuleSet {
            tag_keywords: vec![TagKeywords {
                tag: "area:ssa".into(),
                keywords: vec!["self service".into()],
            }],
            ..Default::default()
        };
        // Keyword is in the BODY, not the title.
        let it = item(|w| {
            w.title = "Transfer ownership".into();
            w.tags = vec![];
            w.description = Some("Lets a user run a Self Service action to move ownership.".into());
        });
        // Title-only: no match. With description opted in: matches area:ssa.
        assert!(suggest_tags(&it, &rules, false).is_empty());
        let s = suggest_tags(&it, &rules, true);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tag, "area:ssa");
    }

    #[test]
    fn suggest_tags_skips_already_applied() {
        use poseiden_core::TagKeywords;
        let rules = RuleSet {
            tag_keywords: vec![TagKeywords {
                tag: "type:bug".into(),
                keywords: vec!["crash".into()],
            }],
            ..Default::default()
        };
        let it = item(|w| {
            w.title = "Crash".into();
            w.tags = vec!["type:bug".into()];
        });
        assert!(suggest_tags(&it, &rules, false).is_empty());
    }

    #[test]
    fn tag_alias_suggests_a_canonical_rewrite_from_a_legacy_tag() {
        use poseiden_core::TagAlias;
        let rules = RuleSet {
            tag_aliases: vec![TagAlias {
                from: "ssa".into(),
                to: "area:ssa".into(),
            }],
            ..Default::default()
        };
        // Legacy "SSA" (any case), no title/body needed -> suggest area:ssa as a REWRITE.
        let it = item(|w| {
            w.tags = vec!["SSA".into()];
        });
        let s = suggest_tags(&it, &rules, false);
        let sug = s
            .iter()
            .find(|x| x.tag == "area:ssa")
            .expect("alias suggestion");
        assert_eq!(sug.replaces.as_deref(), Some("SSA"));
        // Already migrated -> nothing to do.
        let done = item(|w| {
            w.tags = vec!["SSA".into(), "area:ssa".into()];
        });
        assert!(suggest_tags(&done, &rules, false)
            .iter()
            .all(|x| x.tag != "area:ssa"));
        // No matching legacy tag -> no rewrite.
        let clean = item(|w| {
            w.tags = vec!["area:kubernetes".into()];
        });
        assert!(suggest_tags(&clean, &rules, false).is_empty());
    }

    #[test]
    fn contains_word_matches_whole_tokens_not_substrings() {
        assert!(contains_word("build the ado pipeline now", "ado"));
        assert!(!contains_word("versions are adopted here", "ado")); // no substring hit
        assert!(!contains_word("fix this immediately please", "imm")); // "immediately" != imm
        assert!(contains_word("deploy the imm service", "imm"));
        assert!(!contains_word("run the cascade job", "cas"));
        assert!(contains_word("azure devops build failed", "azure devops")); // multi-word
        assert!(contains_word("cluster runs k8s fine", "k8s"));
    }

    // ── Underspecified -> refine ────────────────────────────────────────

    fn refine_rules() -> RuleSet {
        RuleSet {
            required_tags: vec!["area:*".into()],
            refine_tag: Some("to refine".into()),
            refine_min_chars: Some(40),
            ..Default::default()
        }
    }

    #[test]
    fn is_underspecified_only_when_refine_tag_set_and_body_thin() {
        let empty = item(|w| w.description = None);
        let thin = item(|w| w.description = Some("scope TBC".into()));
        let rich = item(|w| w.description = Some("x".repeat(100)));
        assert!(is_underspecified(&empty, &refine_rules()));
        assert!(is_underspecified(&thin, &refine_rules()));
        assert!(!is_underspecified(&rich, &refine_rules()));
        // Feature off (no refine_tag) -> never underspecified, whatever the body.
        assert!(!is_underspecified(&empty, &RuleSet::default()));
    }

    #[test]
    fn underspecified_ignores_pasted_urls_when_measuring_body() {
        // A "see the doc" stub whose only substance is a long URL is NOT substantial:
        // stripping the link leaves a handful of words, well under the threshold.
        let link_stub = item(|w| {
            w.description =
                Some("See [Project overview](https://example.com/docs/:fl:/r/verylongtoken)".into())
        });
        assert!(is_underspecified(&link_stub, &refine_rules()));
    }

    #[test]
    fn underspecified_matches_placeholder_phrases_regardless_of_length() {
        let mut rules = refine_rules();
        rules.refine_phrases = vec!["to be clarified".into()];
        // Real sentence, over the char threshold, but semantically a placeholder - the
        // body literally says the scope is unresolved. The URL is stripped; the phrase
        // still matches (the "empty stub padded by a pasted link" case).
        let placeholder = item(|w| {
            w.description = Some(
                "Scope and project parameters are to be clarified: \
                 [Project overview](https://example.com/x)"
                    .into(),
            )
        });
        assert!(is_underspecified(&placeholder, &rules));
        // Without the phrase configured, the same body is long enough to pass.
        assert!(!is_underspecified(&placeholder, &refine_rules()));
    }

    #[test]
    fn suggest_tags_flags_underspecified_item_to_refine() {
        // Empty body + a missing required area -> suggest the refine tag.
        let it = item(|w| {
            w.title = "Continuity planning epic".into();
            w.tags = vec!["enhancement".into()];
            w.description = None;
        });
        let s = suggest_tags(&it, &refine_rules(), true);
        let r = s
            .iter()
            .find(|x| x.tag == "to refine")
            .expect("refine suggestion");
        assert!(r.reasons[0].contains("underspecified"));
    }

    #[test]
    fn refine_is_not_suggested_on_done_items() {
        // A Resolved/Closed item is DONE - nagging it to "refine" would then trip the
        // stale-when-resolved rule (POSEIDEN contradicting itself). So no refine there,
        // even though the body is thin and a required tag is missing.
        let mut rules = refine_rules();
        rules.resolved_states = vec!["Resolved".into(), "Closed".into()];
        let done = item(|w| {
            w.state = "Resolved".into();
            w.description = None;
            w.tags = vec!["enhancement".into()];
        });
        assert!(is_underspecified(&done, &rules)); // still thin...
        assert!(
            !suggest_tags(&done, &rules, true)
                .iter()
                .any(|x| x.tag == "to refine"),
            "a Resolved item must not be told to refine"
        );
        // The same thin item while still Active DOES get the refine nudge.
        let active = item(|w| {
            w.state = "Active".into();
            w.description = None;
            w.tags = vec!["enhancement".into()];
        });
        assert!(suggest_tags(&active, &rules, true)
            .iter()
            .any(|x| x.tag == "to refine"));
    }

    #[test]
    fn stale_when_resolved_tags_never_suggested_on_done_items_any_source() {
        use poseiden_core::TagAlias;
        // Legacy "Refine" tag on a Closed item: the alias would rewrite it to canonical
        // "to refine", but that's a stale-when-resolved marker - the same contradiction
        // via the alias path (not the refine nudge). Dropped by the final done-item guard.
        let rules = RuleSet {
            resolved_states: vec!["Closed".into()],
            stale_when_resolved_tags: vec!["to refine".into()],
            tag_aliases: vec![TagAlias {
                from: "refine".into(),
                to: "to refine".into(),
            }],
            ..Default::default()
        };
        let done = item(|w| {
            w.state = "Closed".into();
            w.tags = vec!["Refine".into()];
        });
        assert!(
            suggest_tags(&done, &rules, false)
                .iter()
                .all(|x| x.tag != "to refine"),
            "a stale-when-resolved tag must not be suggested on a done item"
        );
        // While Active, canonicalising the legacy tag IS offered.
        let active = item(|w| {
            w.state = "Active".into();
            w.tags = vec!["Refine".into()];
        });
        assert!(suggest_tags(&active, &rules, false)
            .iter()
            .any(|x| x.tag == "to refine"));
    }

    #[test]
    fn suggest_tags_no_refine_when_already_tagged_or_required_satisfied() {
        // Already carries the refine tag -> don't re-suggest it.
        let already = item(|w| {
            w.tags = vec!["to refine".into()];
            w.description = None;
        });
        assert!(suggest_tags(&already, &refine_rules(), true)
            .iter()
            .all(|x| x.tag != "to refine"));
        // Required area already satisfied -> nothing to refine toward.
        let tagged = item(|w| {
            w.tags = vec!["area:kubernetes".into()];
            w.description = None;
        });
        assert!(suggest_tags(&tagged, &refine_rules(), true)
            .iter()
            .all(|x| x.tag != "to refine"));
    }
}
