use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Instance-level POSEIDEN configuration, sourced entirely from the environment
/// ([`ServerConfig::from_env`] / [`TelemetrySettings::from_env`]) - there is no
/// config file. Per-owner config (teams, rules, tags, saved reports) lives in
/// the DB, see [`UserConfig`].
#[derive(Debug, Clone, Default)]
pub struct PoseidenConfig {
    /// HTTP bind address, port, and poll cadence.
    pub server: ServerConfig,
    /// Observability: which log/trace/metric sinks are on and how verbose. The
    /// host binary reads this and calls `poseiden_telemetry::init`.
    pub telemetry: poseiden_telemetry::TelemetrySettings,
}

/// Per-owner configuration - what each user (in a multi-tenant deployment) owns:
/// their teams, hygiene rules, Doctor state, and polling scope. Persisted as a
/// JSON blob keyed by `owner` in the DB. In a standalone/desktop deployment there
/// is exactly one owner (`"default"`). Instance-level settings (`[server]`,
/// telemetry) live separately and are NOT per-owner.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    #[serde(rename = "team", default)]
    pub teams: Vec<TeamConfig>,
    pub rules: RuleSet,
    pub doctor: DoctorConfig,
    /// When false (default), a poll fetches only the active team; true fetches
    /// every configured team. Per-owner, so each user chooses.
    pub poll_all_teams: bool,
}

impl UserConfig {
    /// Effective hygiene ruleset for `team` - its override, else this owner's
    /// default. Mirrors [`PoseidenConfig::rules_for`].
    pub fn rules_for<'a>(&'a self, team: &'a TeamConfig) -> &'a RuleSet {
        team.rules.as_ref().unwrap_or(&self.rules)
    }
}

/// Machine-managed Doctor state. `checks` is the registered set of parameterised
/// health-check keys (crosspose's `additional-checks` pattern), e.g.
/// `"ado-access:Platform Engineering"`. The Doctor's reconciler keeps this in
/// sync with the configured teams automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DoctorConfig {
    pub checks: Vec<String>,
}

/// Server + scheduler settings. Shared by the Docker web instance and the
/// embedded instance inside the Tauri shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address the HTTP API binds to. `127.0.0.1` for the Tauri-embedded
    /// instance (loopback only); `0.0.0.0` in the container.
    pub bind_addr: String,
    pub port: u16,
    /// How often the scheduler polls every configured provider. Human string
    /// (`"15m"`, `"1h"`, `"90s"`); parse with [`parse_duration`].
    pub poll_interval: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".to_string(),
            port: 8737,
            poll_interval: "15m".to_string(),
        }
    }
}

impl ServerConfig {
    /// Parsed poll interval, falling back to 15 minutes if the string is
    /// malformed (a bad value shouldn't stop the server booting - it logs
    /// and uses the default).
    pub fn poll_interval_duration(&self) -> Duration {
        parse_duration(&self.poll_interval).unwrap_or(Duration::from_secs(900))
    }

    /// Build from environment (the instance-level config source now the TOML
    /// file is gone), starting from the defaults:
    /// `POSEIDEN_BIND_ADDR`, `POSEIDEN_PORT`, `POSEIDEN_POLL_INTERVAL`.
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Ok(v) = std::env::var("POSEIDEN_BIND_ADDR") {
            if !v.trim().is_empty() {
                s.bind_addr = v;
            }
        }
        if let Ok(v) = std::env::var("POSEIDEN_PORT") {
            if let Ok(p) = v.trim().parse() {
                s.port = p;
            }
        }
        if let Ok(v) = std::env::var("POSEIDEN_POLL_INTERVAL") {
            if !v.trim().is_empty() {
                s.poll_interval = v;
            }
        }
        s
    }
}

/// Which upstream a project is polled from. Azure DevOps, GitHub, and GitLab
/// ship today; the enum is the extension point for Jira / Linear next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    AzureDevOps,
    /// GitHub - Issues + Pull Requests + Actions. Public repos read without a
    /// token; a token (higher rate limits / private repos) is optional.
    /// `organization` = repo owner, `project` = repo name.
    #[serde(rename = "github")]
    GitHub,
    /// GitLab - Issues + Merge Requests + Pipelines. Public projects read
    /// without a token; a token is optional. `organization` = namespace (or
    /// base URL for self-hosted), `project` = project path.
    #[serde(rename = "gitlab")]
    GitLab,
    /// Deterministic in-memory data - no network, no credential. Powers "demo
    /// mode" (try POSEIDEN without a real tracker) and the end-to-end tests. A
    /// team with `provider = "stub"` returns the same curated work items,
    /// pipelines, and PRs every poll.
    Stub,
}

/// How a project authenticates to its provider. The PAT itself is NEVER stored
/// in config - only the name of the environment variable that holds it. This
/// keeps secrets out of the file, out of git, and out of the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AzureDevOpsAuth {
    /// Env var the Personal Access Token is read from. Defaults to
    /// `POSEIDEN_AZURE_PAT`; override per-project when polling multiple orgs
    /// with different tokens. The token needs only read scopes
    /// (Work Items Read, Build Read) - least privilege for a read-only tool.
    pub pat_env: String,
}

impl Default for AzureDevOpsAuth {
    fn default() -> Self {
        Self {
            pat_env: "POSEIDEN_AZURE_PAT".to_string(),
        }
    }
}

/// One team to give visibility into - a named board backed by a provider
/// project (optionally narrowed to an area path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    /// The team's display name - used throughout POSEIDEN (the scope selector,
    /// dashboards, flags, the `team` field on every normalised entity). Distinct
    /// from the provider's project name, so "Platform Engineering" can map to
    /// a project called `Platform` (or share a project with another team via a
    /// different `area_path`).
    pub name: String,
    #[serde(default)]
    pub provider: ProviderKind,
    /// Provider organisation URL, e.g. `https://dev.azure.com/acme` for Azure
    /// DevOps.
    pub organization: String,
    /// Provider project name within the organisation.
    pub project: String,
    /// Entra ID tenant this org belongs to - a domain (`acme.com`), a
    /// `*.onmicrosoft.com` name, or a tenant GUID. Passed to `az login` and
    /// `az account get-access-token` as `--tenant` so sign-in targets the work/
    /// school account directly instead of the consumer endpoint. Omit for a
    /// PAT, or when your default `az` context is already the right tenant.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Optional area path to scope this team's board *within* the project, e.g.
    /// `"Platform\\Networking"`. When set, only work items UNDER this area path
    /// are pulled - this is what lets two POSEIDEN teams share one Azure DevOps
    /// project and still show distinct boards. When absent, the whole project's
    /// work items are the team's board.
    #[serde(default)]
    pub area_path: Option<String>,
    /// Restrict `area_path` to the *exact* path rather than including child
    /// areas. Default `false` = `UNDER` (children included, the usual board
    /// scope); `true` = `[System.AreaPath] = '<path>'` only. Ignored without an
    /// `area_path` or with a `wiql` override.
    #[serde(default)]
    pub area_path_strict: bool,
    #[serde(default)]
    pub auth: AzureDevOpsAuth,
    /// Optional WIQL override. When absent, POSEIDEN builds a default query that
    /// pulls non-removed work items in the project (narrowed by `area_path` if
    /// set). Providing this takes full control - `area_path` is then ignored.
    #[serde(default)]
    pub wiql: Option<String>,
    /// Pipeline/definition ids to monitor for this team. Empty = every pipeline
    /// in the project. Naming a subset scopes "owned pipelines" to the team's.
    #[serde(default)]
    pub pipeline_ids: Vec<i64>,
    /// Optional per-team hygiene ruleset. Different teams run different processes
    /// (one may mandate `type:*` tags and a 5-day WIP limit while another is far
    /// looser), so each team can carry its own `[team.rules]` block. When absent,
    /// the team inherits the instance-wide `[rules]` default; see
    /// [`PoseidenConfig::rules_for`]. It's a whole-ruleset override, not a field
    /// merge: a `[team.rules]` block replaces the default outright (an empty list
    /// there means "no rule", which a field merge couldn't express unambiguously).
    #[serde(default)]
    pub rules: Option<RuleSet>,
}

/// The hygiene ruleset. Tag patterns support a trailing `*` wildcard:
/// `"type:*"` matches any tag beginning `type:`. Exact strings match exactly.
/// Comparison is case-insensitive (provider tags are typically case-preserving
/// but case-insensitive).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSet {
    /// Every item must carry at least one tag matching each pattern here.
    /// `["team:*", "type:*"]` means "must have a team: tag AND a type: tag".
    pub required_tags: Vec<String>,
    /// Allow-list. When non-empty, any tag NOT matching one of these patterns
    /// is flagged as disallowed. Empty = allow anything (allow-list disabled).
    pub allowed_tags: Vec<String>,
    /// Explicit deny-list. Any tag matching one of these is flagged, even if
    /// `allowed_tags` is empty. `["wip", "temp", "do-not-merge"]`.
    pub disallowed_tags: Vec<String>,
    /// Auto-suggest keywords per tag: when a work item's title contains any of a
    /// tag's keywords (case-insensitive) and it lacks that tag, the tag is
    /// suggested with the matching keyword(s) as the reason. Purely advisory -
    /// the tag string is still what's used everywhere else.
    #[serde(default)]
    pub tag_keywords: Vec<TagKeywords>,
    /// Tag rewrite/alias rules: an existing tag matching `from` suggests the canonical
    /// `to` as a REWRITE (apply = add `to`, drop the matched `from`). The non-AI
    /// migration path - e.g. `SSA` -> `area:ssa` - for users who can't/won't run a
    /// model. Matched case-insensitively; `from` may end in `*` (prefix).
    #[serde(default)]
    pub tag_aliases: Vec<TagAlias>,
    /// Treat a completely untagged item as an error (`true`) or a warning
    /// (`false`). Default warning.
    pub untagged_is_error: bool,
    /// Per-state staleness limits. An item in state `S` whose last change is
    /// older than `days[S]` is flagged stale. States absent from the map are
    /// never stale. Example: `{ "In Progress" = 5, "New" = 14 }`.
    pub stale_days: BTreeMap<String, i64>,
    /// States exempt from ALL hygiene checks - typically terminal states you
    /// don't care about tagging (`["Closed", "Done", "Removed"]`).
    pub ignore_states: Vec<String>,
    /// States that mean the item is DONE. A tag implying outstanding work (see
    /// `stale_when_resolved_tags`) is contradictory here, so it's flagged. This is
    /// the ONE check that deliberately runs even on `ignore_states` - a terminal
    /// state is exactly where a leftover "still needs work" tag is worth surfacing.
    #[serde(default = "default_resolved_states")]
    pub resolved_states: Vec<String>,
    /// Tags that mean "still has outstanding work" ("to refine", "to do", "needs
    /// triage", ...). Flagged (`StaleStateTag`) when the item is in a
    /// `resolved_states` state. Matched like other tag patterns (case-insensitive,
    /// glob). Empty disables the check.
    #[serde(default = "default_stale_when_resolved_tags")]
    pub stale_when_resolved_tags: Vec<String>,
    /// Work-item types exempt from all hygiene checks (`["Task"]` if you only
    /// govern stories/bugs).
    pub ignore_types: Vec<String>,
    /// Pipeline hygiene checks (a sub-policy of the same ruleset, so it inherits
    /// / overrides per-team exactly like the work-item rules above).
    pub pipelines: PipelineRules,
    /// Pull-request hygiene checks.
    pub pull_requests: PrRules,
}

/// Terminal states seeded by default so the stale-tag check works out of the box.
/// Override per team (or empty `resolved_states` to disable).
fn default_resolved_states() -> Vec<String> {
    ["Closed", "Done", "Resolved", "Completed", "Removed"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// "Still needs work" tags seeded by default - contradictory once an item is in a
/// resolved state. Override per team (or empty to disable the check).
fn default_stale_when_resolved_tags() -> Vec<String> {
    ["to refine", "to do", "needs triage", "in progress", "blocked", "wip"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Keywords that auto-suggest a tag. See [`RuleSet::tag_keywords`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TagKeywords {
    /// The tag to suggest (the exact string used everywhere else).
    pub tag: String,
    /// Trigger words; a case-insensitive substring match on the item's title.
    pub keywords: Vec<String>,
}

/// A tag rewrite rule: a legacy tag `from` is suggested to be replaced by the
/// canonical `to`. See [`RuleSet::tag_aliases`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TagAlias {
    /// Legacy tag to match on the item (case-insensitive; trailing `*` = prefix).
    pub from: String,
    /// Canonical tag to suggest in its place.
    pub to: String,
}

/// Hygiene checks for CI/CD pipelines. Each is opt-in (off by default).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineRules {
    /// Flag a pipeline whose most recent run failed.
    pub flag_failing: bool,
    /// Flag a pipeline that has never run (defined but never executed).
    pub flag_never_run: bool,
}

/// Hygiene checks for pull requests. A `None`/`0` threshold disables that check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PrRules {
    /// Flag an active (non-draft) PR open longer than this many days.
    pub stale_open_days: Option<i64>,
    /// Flag a draft PR open longer than this many days.
    pub stale_draft_days: Option<i64>,
    /// Flag an active PR that has no linked work item (traceability).
    pub require_work_item: bool,
    /// Show abandoned PRs among a work item's linked-PR chips. Off by default -
    /// abandoned links are noise unless a team wants to see them.
    pub link_include_abandoned: bool,
}

/// Convert a per-state staleness entry into a typed pair. Convenience for the
/// rules engine; keeps the `(state, days)` shape explicit at call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRule {
    pub state: String,
    pub days: i64,
}

/// Parse a human duration string into a [`Duration`]. Supports a single
/// unit suffix: `s` (seconds), `m` (minutes), `h` (hours), `d` (days). A bare
/// number is treated as seconds. Returns `None` on anything unparseable.
///
/// ```
/// # use poseiden_core::parse_duration;
/// use std::time::Duration;
/// assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
/// assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
/// assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
/// assert_eq!(parse_duration("nonsense"), None);
/// ```
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit): (&str, u64) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .map(|n| Duration::from_secs(n * unit))
}

impl RuleSet {
    /// The staleness rules as typed pairs, for the engine to iterate.
    pub fn stale_rules(&self) -> Vec<StaleRule> {
        self.stale_days
            .iter()
            .map(|(state, days)| StaleRule {
                state: state.clone(),
                days: *days,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("15x"), None);
        assert_eq!(parse_duration("m"), None);
    }

    #[test]
    fn defaults_are_sane() {
        // Instance defaults (env-sourced at runtime; these are the fallbacks).
        let s = ServerConfig::default();
        assert_eq!(s.port, 8737);
        assert_eq!(s.bind_addr, "127.0.0.1");
        // A fresh owner's config is empty - no seed. Guards the `#[serde(default)]`
        // wiring the per-owner blob relies on.
        let empty: UserConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(empty.teams.is_empty());
        assert!(empty.rules.required_tags.is_empty());
    }

    #[test]
    fn user_config_parses_teams() {
        // The DB stores per-owner config as this JSON shape (the `team` key holds
        // the list). Two teams: a whole project, and an area-path slice of a
        // *different* project with its own PAT env + pipeline subset.
        let json = serde_json::json!({
            "team": [
                { "name": "Platform Engineering", "organization": "https://dev.azure.com/acme", "project": "Platform Engineering" },
                { "name": "DevOps", "organization": "https://dev.azure.com/acme", "project": "Platform",
                  "area_path": "Platform\\DevOps", "pipeline_ids": [12, 34],
                  "auth": { "pat_env": "ACME_DEVOPS_PAT" } }
            ],
            "rules": { "required_tags": ["team:*", "type:*"], "untagged_is_error": true }
        });
        let cfg: UserConfig = serde_json::from_value(json).expect("config parses");
        assert_eq!(cfg.teams.len(), 2);
        assert_eq!(cfg.teams[0].name, "Platform Engineering");
        assert!(cfg.teams[0].area_path.is_none());
        assert_eq!(cfg.teams[1].area_path.as_deref(), Some(r"Platform\DevOps"));
        assert_eq!(cfg.teams[1].auth.pat_env, "ACME_DEVOPS_PAT");
        assert_eq!(cfg.teams[1].pipeline_ids, vec![12, 34]);
        // The first team keeps the default PAT env + provider.
        assert_eq!(cfg.teams[0].auth.pat_env, "POSEIDEN_AZURE_PAT");
        assert_eq!(cfg.teams[0].provider, ProviderKind::AzureDevOps);
    }

    #[test]
    fn per_team_rules_override_else_inherit_global() {
        // A team with no `rules` inherits the instance default; a team with its
        // own overrides it wholesale. Different teams, different policy - the core
        // of the multi-team design.
        let json = serde_json::json!({
            "team": [
                { "name": "Inheritor", "organization": "o", "project": "A" },
                { "name": "Override", "organization": "o", "project": "B",
                  "rules": { "required_tags": ["type:*"] } }
            ],
            "rules": { "required_tags": ["team:*"] }
        });
        let cfg: UserConfig = serde_json::from_value(json).unwrap();
        let inheritor = &cfg.teams[0];
        let overrider = &cfg.teams[1];
        assert!(inheritor.rules.is_none(), "no block means inherit");
        assert!(overrider.rules.is_some(), "a block means override");
        // Inheritor resolves to the global default; overrider to its own.
        assert_eq!(cfg.rules_for(inheritor).required_tags, vec!["team:*"]);
        assert_eq!(cfg.rules_for(overrider).required_tags, vec!["type:*"]);
    }

    #[test]
    fn per_team_empty_rules_block_replaces_not_merges() {
        // The documented contract: a `[team.rules]` block is a WHOLE-ruleset
        // override, not a field merge. An empty `required_tags` in the override
        // therefore means "no required tags", NOT "fall back to the global".
        let json = serde_json::json!({
            "team": [
                { "name": "Loose", "organization": "o", "project": "A",
                  "rules": { "required_tags": [] } }
            ],
            "rules": { "required_tags": ["team:*", "type:*"] }
        });
        let cfg: UserConfig = serde_json::from_value(json).unwrap();
        let loose = &cfg.teams[0];
        assert!(loose.rules.is_some());
        // Resolves to the (empty) override, not the two global required tags.
        assert!(cfg.rules_for(loose).required_tags.is_empty());
    }

    #[test]
    fn ruleset_seeds_resolved_and_stale_defaults() {
        // A minimal RuleSet gets the documented terminal-state + stale-tag seeds
        // so the StaleStateTag check works out of the box. This pins the two
        // `#[serde(default = "...")]` fields.
        let rs: RuleSet = serde_json::from_str("{}").expect("empty ruleset parses");
        assert_eq!(
            rs.resolved_states,
            vec!["Closed", "Done", "Resolved", "Completed", "Removed"]
        );
        assert_eq!(
            rs.stale_when_resolved_tags,
            vec!["to refine", "to do", "needs triage", "in progress", "blocked", "wip"]
        );
        // Fields without a custom default are empty/false.
        assert!(rs.required_tags.is_empty());
        assert!(rs.allowed_tags.is_empty());
        assert!(rs.disallowed_tags.is_empty());
        assert!(rs.tag_keywords.is_empty());
        assert!(rs.tag_aliases.is_empty());
        assert!(!rs.untagged_is_error);
        assert!(rs.stale_days.is_empty());
    }

    #[test]
    fn ruleset_empty_resolved_states_stays_empty() {
        // Explicitly providing an empty list disables the seed (and the check) -
        // the default only fills in when the key is ABSENT.
        let rs: RuleSet =
            serde_json::from_str(r#"{ "resolved_states": [], "stale_when_resolved_tags": [] }"#)
                .expect("parses");
        assert!(rs.resolved_states.is_empty());
        assert!(rs.stale_when_resolved_tags.is_empty());
    }

    #[test]
    fn ruleset_roundtrips_populated_fields() {
        // A fully-populated RuleSet survives serialize -> deserialize unchanged.
        // RuleSet has no PartialEq, so compare via the JSON Value both ways.
        let json = serde_json::json!({
            "required_tags": ["team:*"],
            "allowed_tags": ["type:*", "area:*"],
            "disallowed_tags": ["wip", "temp"],
            "tag_keywords": [ { "tag": "type:bug", "keywords": ["crash", "error"] } ],
            "tag_aliases": [ { "from": "SSA", "to": "area:ssa" } ],
            "untagged_is_error": true,
            "stale_days": { "In Progress": 5, "New": 14 },
            "ignore_states": ["Closed"],
            "resolved_states": ["Done"],
            "stale_when_resolved_tags": ["to do"],
            "ignore_types": ["Task"]
        });
        let rs: RuleSet = serde_json::from_value(json.clone()).expect("parses");
        // Spot-check parsed fields.
        assert_eq!(rs.tag_keywords.len(), 1);
        assert_eq!(rs.tag_keywords[0].tag, "type:bug");
        assert_eq!(rs.tag_aliases[0], TagAlias { from: "SSA".into(), to: "area:ssa".into() });
        assert_eq!(rs.disallowed_tags, vec!["wip", "temp"]);
        assert_eq!(rs.resolved_states, vec!["Done"]);
        assert_eq!(rs.stale_days.get("In Progress"), Some(&5));
        // Round-trip: reserialize and confirm it re-parses to the same values.
        let back = serde_json::to_value(&rs).expect("serialises");
        let rs2: RuleSet = serde_json::from_value(back).expect("re-parses");
        assert_eq!(rs2.tag_aliases, rs.tag_aliases);
        assert_eq!(rs2.tag_keywords, rs.tag_keywords);
        assert_eq!(rs2.disallowed_tags, rs.disallowed_tags);
        assert_eq!(rs2.resolved_states, rs.resolved_states);
        assert_eq!(rs2.required_tags, rs.required_tags);
    }

    #[test]
    fn stale_rules_pairs_from_map() {
        // stale_rules() flattens the BTreeMap into typed (state, days) pairs,
        // in the map's sorted-key order.
        let rs: RuleSet =
            serde_json::from_str(r#"{ "stale_days": { "New": 14, "In Progress": 5 } }"#)
                .expect("parses");
        let rules = rs.stale_rules();
        assert_eq!(rules.len(), 2);
        // BTreeMap iterates sorted by key: "In Progress" < "New".
        assert_eq!(rules[0], StaleRule { state: "In Progress".into(), days: 5 });
        assert_eq!(rules[1], StaleRule { state: "New".into(), days: 14 });
    }

    #[test]
    fn tag_alias_defaults_and_parse() {
        // TagAlias fields both default to empty strings when absent.
        let empty: TagAlias = serde_json::from_str("{}").expect("parses");
        assert_eq!(empty, TagAlias { from: String::new(), to: String::new() });
        // And parse from a normal rule.
        let a: TagAlias =
            serde_json::from_str(r#"{ "from": "legacy*", "to": "area:x" }"#).expect("parses");
        assert_eq!(a.from, "legacy*");
        assert_eq!(a.to, "area:x");
    }

    #[test]
    fn provider_kind_serde_is_kebab_case() {
        // Stub vs the default AzureDevOps, on the wire as kebab-case.
        assert_eq!(
            serde_json::to_value(ProviderKind::AzureDevOps).unwrap(),
            serde_json::json!("azure-dev-ops")
        );
        assert_eq!(
            serde_json::to_value(ProviderKind::Stub).unwrap(),
            serde_json::json!("stub")
        );
        let k: ProviderKind = serde_json::from_value(serde_json::json!("stub")).unwrap();
        assert_eq!(k, ProviderKind::Stub);
    }

    #[test]
    fn poll_interval_duration_falls_back_on_garbage() {
        // A malformed interval must not stop the server booting - it uses 15m.
        let s = ServerConfig { poll_interval: "nonsense".to_string(), ..Default::default() };
        assert_eq!(s.poll_interval_duration(), Duration::from_secs(900));
        let s = ServerConfig { poll_interval: "30s".to_string(), ..Default::default() };
        assert_eq!(s.poll_interval_duration(), Duration::from_secs(30));
    }
}
