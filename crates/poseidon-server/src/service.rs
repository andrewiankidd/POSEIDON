//! The shared `Service` - POSEIDON's business logic, transport-agnostic.
//!
//! This is the crate's reason for existing: one object that owns the config +
//! store and exposes every operation the app performs (poll, dashboard,
//! tickets, pipelines, reports). The axum handlers call it; the Tauri invoke
//! handlers call it; the CLI calls it. Logic lives here exactly once, so the
//! three delivery shells can never drift.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use poseidon_core::{
    BundleMeta, CatalogEntity, ConfigBundle, DashboardSummary, EntityFlag, Flag, FlagCode,
    LinkedPr, PipelineHealth, PipelineReport, PoseidonConfig, PrStatus, PullRequest, RunStatus,
    Severity, TagCount, TeamConfig, TicketReport, WorkItem, WorkItemUpdate, CONFIG_BUNDLE_SCHEMA,
    DEFAULT_OWNER,
};
use poseidon_doctor::{Check, Doctor, DoctorReport, FixResult};
use poseidon_providers::{CatalogSource, Credential, CsvCatalog, FieldMap};
use poseidon_store::Store;
use tracing::{info, warn};

use crate::checks::{AzureDevOpsAccessCheck, TeamCheckReconciler, UpdateCheck};
use crate::config_store::ConfigStore;

/// How far back pipeline health looks when folding run history into a status.
const HEALTH_WINDOW_DAYS: i64 = 90;
/// Max work-item origins fetched per poll (the "moved in" backfill). Caps the
/// one-time backfill's API cost; it spreads across polls since origins are immutable.
const ORIGIN_BACKFILL_PER_POLL: i64 = 250;
/// Meta key holding the UI's currently-selected team, so the active-team poll
/// knows what to fetch when `poll_all_teams` is off.
const ACTIVE_TEAM_KEY: &str = "active_team";

/// Whether POSEIDON can currently authenticate to the provider, and how. Drives
/// the UI's "Sign in" banner. `signed_in == false` means every poll will fail
/// until the user signs in (`az login`) or sets a PAT.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthStatus {
    pub signed_in: bool,
    /// `"pat"` or `"az"` when signed in; `None` otherwise.
    pub method: Option<String>,
    pub message: String,
}

/// Summary of one poll pass - surfaced to the CLI's `poll` command and the
/// API's manual-refresh endpoint so a human can see what happened.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PollOutcome {
    pub teams_polled: usize,
    pub work_items: usize,
    pub pipelines: usize,
    pub runs: usize,
    pub pull_requests: usize,
    /// Per-team failures - a poll never aborts the whole run because one team's
    /// PAT is missing or its API is down; it records the error, skips that
    /// team, and carries on.
    pub errors: Vec<String>,
}

/// What a config import changed. Counts are rows written: all of them on a
/// The result of an AI field-draft request. Either the SERVER produced the text (an
/// online provider ran it), or - when the only configured model is browser-run
/// (WebGPU) - the built prompt is handed back for the BROWSER to run through the same
/// model the tagger uses. This is what lets drafting reuse the tagger's AI without a
/// separate "online model" requirement.
#[derive(Debug, Clone)]
pub enum DraftOutcome {
    Value(String),
    Prompt { system: String, user: String },
}

/// The result of a whole-item consistency sweep. Like [`DraftOutcome`] but the value
/// is a SET of field changes: either the server produced them, or the built prompt is
/// handed back for the browser (WebGPU) to run - the browser then posts the reply to
/// [`Service::parse_refine_reply`] to be validated + turned into changes server-side.
#[derive(Debug, Clone)]
pub enum RefineOutcome {
    Value(Vec<poseidon_core::FieldChange>),
    Prompt { system: String, user: String },
}

/// `replace`, only the newly-added ones on a merge.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportSummary {
    pub teams: usize,
    pub reports: usize,
    pub replaced: bool,
}

/// Per-owner AI taggers: each owner maps to their configured tagger, or `None`
/// when AI is not enabled for them. `Option` distinguishes "not yet built" from
/// "built, none configured".
type OwnerTaggers = HashMap<String, Option<Arc<dyn poseidon_ai::AiTagger>>>;

/// The transport-agnostic logic layer. Cheap to clone (all fields Arc-backed),
/// which is how a hosted request re-scopes it to the authenticated owner.
#[derive(Clone)]
pub struct Service {
    config: ConfigStore,
    store: Store,
    owner: String,
    /// Root under which each hosted owner gets an isolated `AZURE_CONFIG_DIR`
    /// (`<root>/<owner>/`). On the data volume so per-user `az login` sessions are
    /// isolated and survive restarts. The `default` owner ignores this and uses
    /// the machine `~/.azure` instead.
    az_config_root: Arc<std::path::PathBuf>,
    /// In-flight / last web device-code sign-in state, per owner. Standard mutex
    /// (never held across `.await`): the `az login` progress callback writes it
    /// synchronously and the status endpoint reads it.
    signins: Arc<std::sync::Mutex<HashMap<String, SigninState>>>,
    /// Optional access-token verifier. When set, the HTTP transport derives the
    /// owner from a verified token instead of trusting the identity header (see
    /// [`crate::token`]). `Arc<Option<_>>` so `with_owner` clones keep it shared.
    verifier: Arc<Option<crate::token::TokenVerifier>>,
    /// Per-owner AI taggers, built lazily from each owner's config and cached.
    /// Multi-tenant: each owner picks their own provider/model/key (Alice on
    /// Claude, Bob on a local model). Offline models are shared process-wide by id
    /// (see `poseidon_ai::embedded`), so per-owner taggers don't multiply memory.
    ai: Arc<std::sync::RwLock<OwnerTaggers>>,
    /// In-flight / last AI tag-suggestion run, per owner (a background job). Same
    /// non-`.await`-held mutex discipline as `signins`.
    suggest_jobs: Arc<std::sync::Mutex<HashMap<String, SuggestState>>>,
    /// In-flight / last on-demand AI healthcheck audit run, per owner (a background
    /// job). Same non-`.await`-held mutex discipline as `suggest_jobs`.
    audit_jobs: Arc<std::sync::Mutex<HashMap<String, AuditState>>>,
}

/// State of an owner's web (hosted) device-code sign-in. Backs `GET /api/sign-in`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SigninState {
    /// No web sign-in has been started for this owner this process.
    Idle,
    /// `az login` is running; the user should open `url` and enter `code`.
    Pending { url: String, code: String },
    /// The last sign-in completed successfully.
    Done,
    /// The last sign-in failed.
    Failed { error: String },
}

/// State of an owner's AI tag-suggestion run. Backs `GET /api/tag-suggestions/status`.
/// Model inference is slow (and, offline, very slow), so the browser starts the run
/// as a background job and polls this - rather than blocking one request past the
/// proxy's read timeout (which produced a 504 over a big backlog).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SuggestState {
    /// No run has been started for this owner this process.
    Idle,
    /// A run is in progress: `done`/`total` items processed, `suggestions` stored so far.
    Running {
        done: usize,
        total: usize,
        suggestions: usize,
    },
    /// The last run finished.
    Done { summary: AiSuggestSummary },
    /// The last run failed.
    Failed { error: String },
}

/// State of an owner's on-demand AI healthcheck audit run. Backs
/// `GET /api/healthcheck/audit/status`. Same background-job rationale as
/// [`SuggestState`]: judging each item with the model is slow, so the browser
/// starts it and polls rather than blocking one request past the proxy timeout.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AuditState {
    /// No audit has been started for this owner this process.
    Idle,
    /// A run is in progress: `done`/`total` items judged, `findings` stored so far.
    Running {
        done: usize,
        total: usize,
        findings: usize,
    },
    /// The last audit finished.
    Done { summary: AiAuditSummary },
    /// The last audit failed.
    Failed { error: String },
}

/// Map an owner key to a filesystem-safe directory name for its isolated `az`
/// cache. The owner is already normalised (lower-cased, trimmed); we keep the
/// unreserved set and collapse everything else to `_`, so it stays readable for
/// ops (`a.user_example.com`) while never escaping the sessions root.
fn sanitize_owner(owner: &str) -> String {
    let mapped: String = owner
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Never yield an empty or dots-only component (`.`, `..`) - as a single path
    // segment that would resolve to the sessions root or its parent.
    if mapped.is_empty() || mapped.chars().all(|c| c == '.') {
        "_".to_string()
    } else {
        mapped
    }
}

/// Normalise an owner key (from the auth header). Lower-cased + trimmed; empty
/// falls back to the single-tenant [`DEFAULT_OWNER`] so unauthenticated /
/// standalone use is unchanged.
fn normalise_owner(s: &str) -> String {
    let t = s.trim().to_lowercase();
    if t.is_empty() {
        DEFAULT_OWNER.to_string()
    } else {
        t
    }
}

impl Service {
    /// Build a service over an already-connected store. Per-owner config now
    /// lives in the DB (`ConfigStore`); the bootstrap `config` seeds the `default`
    /// owner on first run and supplies instance-level settings. `owner` is fixed
    /// to [`DEFAULT_OWNER`] until the per-request owner (Stage 2) lands.
    pub fn new(config: PoseidonConfig, store: Store, az_config_root: std::path::PathBuf) -> Self {
        // A fresh `default` owner starts empty; config comes from the UI or
        // `config import`, not a seed file.
        let cfg = ConfigStore::new(
            store.clone(),
            config.server.clone(),
            poseidon_core::UserConfig::default(),
        );
        Self {
            config: cfg,
            store,
            owner: DEFAULT_OWNER.to_string(),
            az_config_root: Arc::new(az_config_root),
            signins: Arc::new(std::sync::Mutex::new(HashMap::new())),
            verifier: Arc::new(None),
            ai: Arc::new(std::sync::RwLock::new(HashMap::new())),
            suggest_jobs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            audit_jobs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Attach an optional forwarded-token verifier (built from the environment at
    /// startup). `None` leaves the default header-trust behaviour in place.
    pub fn with_verifier(mut self, verifier: Option<crate::token::TokenVerifier>) -> Self {
        self.verifier = Arc::new(verifier);
        self
    }

    /// Whether an AI tag suggester is configured for THIS owner (for the UI).
    pub async fn ai_enabled(&self) -> bool {
        self.ai_tagger().await.is_some()
    }

    /// This owner's LLM integration registry. A saved registry (`llm_config`) wins and
    /// is authoritative (the user's own order/keys). With none saved, a fresh owner
    /// gets the seeded default catalog (the full multiplatform range) so the settings
    /// UI shows the options up front - greyed where this platform can't run them. A
    /// deployment that injects an AI endpoint via env (the chart's bundled Ollama) is
    /// prepended as the highest-priority entry, so it stays active out of the box.
    async fn stored_llm_config(&self) -> poseidon_ai::LlmConfig {
        if let Ok(Some(json)) = self.store.get_meta(&self.owner, "llm_config").await {
            if let Ok(cfg) = serde_json::from_str::<poseidon_ai::LlmConfig>(&json) {
                return cfg;
            }
        }
        // No saved registry: size the seeded catalog to THIS process's caps (CPU
        // cores, CUDA) so a desktop/CLI auto-picks a sensible model; the web client
        // refines it (adding WebGPU + client caps) via the autotune endpoint.
        let mut cfg = poseidon_ai::LlmConfig::seeded_for(&poseidon_ai::PlatformCaps::server());
        let env = poseidon_ai::AiConfig::from_env();
        if env.enabled() {
            let mut env_reg = poseidon_ai::LlmConfig::from_single(env).integrations;
            env_reg.append(&mut cfg.integrations);
            cfg.integrations = env_reg;
        }
        cfg
    }

    /// Auto-configure this owner's LLM registry for the detected platform, sizing each
    /// local model to capability ([`poseidon_ai::recommend_model`]). Only (re)writes
    /// when the registry is unsaved or was itself auto-configured - a hand-edited
    /// registry (`auto = false`) is never touched. `browser` carries client-only caps
    /// (WebGPU availability, client RAM/cores) merged over the server's own. Returns
    /// the effective registry view (same shape as [`Self::llm_config_view`]).
    pub async fn autotune_llm_config(
        &self,
        browser: poseidon_ai::PlatformCaps,
    ) -> serde_json::Value {
        let stored = self
            .store
            .get_meta(&self.owner, "llm_config")
            .await
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<poseidon_ai::LlmConfig>(&j).ok());
        let is_manual = stored.as_ref().map(|c| !c.auto).unwrap_or(false);
        if !is_manual {
            let caps = poseidon_ai::PlatformCaps::server().merged_with_browser(&browser);
            let mut tuned = poseidon_ai::LlmConfig::seeded_for(&caps);
            let env = poseidon_ai::AiConfig::from_env();
            if env.enabled() {
                let mut env_reg = poseidon_ai::LlmConfig::from_single(env).integrations;
                env_reg.append(&mut tuned.integrations);
                tuned.integrations = env_reg;
            }
            if let Ok(json) = serde_json::to_string(&tuned) {
                let _ = self.store.set_meta(&self.owner, "llm_config", &json).await;
                self.invalidate_ai();
            }
        }
        self.llm_config_view().await
    }

    /// This owner's tagger, built from their registry and cached. Resolution picks
    /// the first integration (by priority) compatible with THIS platform's caps, so
    /// a GPU-offline entry wins on a CUDA box and falls through to an online entry on
    /// the CPU pod. Rebuilt after a settings change (via [`Self::invalidate_ai`]).
    async fn ai_tagger(&self) -> Option<Arc<dyn poseidon_ai::AiTagger>> {
        {
            let cache = self.ai.read().unwrap();
            if let Some(cached) = cache.get(&self.owner) {
                return cached.clone();
            }
        } // drop the read guard before awaiting
        let built = self
            .stored_llm_config()
            .await
            .resolve(&poseidon_ai::PlatformCaps::server());
        self.ai
            .write()
            .unwrap()
            .insert(self.owner.clone(), built.clone());
        built
    }

    /// Drop this owner's cached tagger so it rebuilds from the current config.
    fn invalidate_ai(&self) {
        self.ai.write().unwrap().remove(&self.owner);
    }

    /// The LLM integration registry for the settings UI: every integration with its
    /// API key redacted to a presence sentinel, annotated with whether it's
    /// `compatible` with THIS platform and which one is `active` (first compatible in
    /// priority order), plus the platform caps and provider/model presets. Never
    /// leaks a key to the browser.
    pub async fn llm_config_view(&self) -> serde_json::Value {
        let cfg = self.stored_llm_config().await;
        let caps = poseidon_ai::PlatformCaps::server();
        let active = cfg.active_id(&caps).map(|s| s.to_string());
        let integrations: Vec<serde_json::Value> = cfg
            .integrations
            .iter()
            .map(|i| {
                let mut v = serde_json::to_value(i).unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    let has_key = i.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false);
                    // "" = a key is stored (leave blank to keep); null = none.
                    obj.insert(
                        "api_key".to_string(),
                        if has_key {
                            serde_json::json!("")
                        } else {
                            serde_json::Value::Null
                        },
                    );
                    obj.insert(
                        "compatible".to_string(),
                        serde_json::json!(i.compatible(&caps)),
                    );
                    obj.insert("configured".to_string(), serde_json::json!(i.configured()));
                    obj.insert(
                        "active".to_string(),
                        serde_json::json!(active.as_deref() == Some(i.id.as_str())),
                    );
                }
                v
            })
            .collect();
        serde_json::json!({
            "integrations": integrations,
            "caps": caps,
            "active_id": active,
            "presets": { "online": poseidon_ai::ONLINE_PROVIDERS, "offline": poseidon_ai::OFFLINE_MODELS },
        })
    }

    /// Persist this owner's LLM registry and rebuild their tagger. A blank `api_key`
    /// on an integration keeps its previously stored key (matched by id), so "leave
    /// blank to keep" works per integration.
    pub async fn set_llm_config(&self, mut cfg: poseidon_ai::LlmConfig) -> anyhow::Result<()> {
        // A hand-edited save is by definition manual - clear the auto flag so autotune
        // never overwrites the user's choice afterwards.
        cfg.auto = false;
        let stored = self.stored_llm_config().await;
        for i in &mut cfg.integrations {
            if i.api_key.as_deref().unwrap_or("").is_empty() {
                i.api_key = stored
                    .integrations
                    .iter()
                    .find(|s| s.id == i.id)
                    .and_then(|s| s.api_key.clone());
            }
        }
        let json = serde_json::to_string(&cfg)?;
        self.store
            .set_meta(&self.owner, "llm_config", &json)
            .await?;
        self.invalidate_ai();
        Ok(())
    }

    /// Drop this owner's saved registry so it reverts to the seeded default catalog
    /// (the full multiplatform range). Used by the settings "Reset to defaults".
    pub async fn reset_llm_config(&self) -> anyhow::Result<()> {
        self.store.delete_meta(&self.owner, "llm_config").await?;
        self.invalidate_ai();
        Ok(())
    }

    /// Whether the owner opted in to feeding work-item descriptions to the tagger
    /// (AI + keyword). Default TRUE (richer signal); flip off to keep bodies out of
    /// the prompt - notably so they don't leave the box for a cloud backend.
    pub async fn tag_use_description(&self) -> bool {
        match self
            .store
            .get_meta(&self.owner, "tag_use_description")
            .await
        {
            Ok(Some(v)) => v != "false",
            _ => true,
        }
    }

    /// Persist the description-in-tagging toggle.
    pub async fn set_tag_use_description(&self, on: bool) -> anyhow::Result<()> {
        self.store
            .set_meta(
                &self.owner,
                "tag_use_description",
                if on { "true" } else { "false" },
            )
            .await?;
        Ok(())
    }

    /// Descriptions (HTML-stripped text) for the given work-item ids. The WebGPU tagger
    /// runs client-side and needs the body pushed to it (the tickets list omits it, to
    /// stay lean). Owner-scoped; empty when the owner opted out of description tagging.
    pub async fn work_item_descriptions(
        &self,
        team: Option<&str>,
        ids: &[i64],
    ) -> anyhow::Result<std::collections::HashMap<i64, String>> {
        if !self.tag_use_description().await {
            return Ok(Default::default());
        }
        let want: std::collections::HashSet<i64> = ids.iter().copied().collect();
        let items = self.store.list_work_items(&self.owner, team).await?;
        Ok(items
            .into_iter()
            .filter(|it| want.contains(&it.id))
            .filter_map(|it| it.description.map(|d| (it.id, d)))
            .collect())
    }

    /// Time one fixed test query against every server-runnable, configured integration
    /// in the registry. Runs them concurrently, each under a timeout so a slow local
    /// model can't stall the whole request. WebGPU entries run in the browser, so they
    /// are returned as `webgpu` candidates for the client to time, not run here.
    /// Returns `{ results: [{id,name,kind,status,ms,tags,error}], webgpu: [...] }`.
    pub async fn benchmark_llms(&self) -> serde_json::Value {
        use std::time::{Duration, Instant};
        // Measured out-of-app (release): on 16 host cores the 0.5B CPU model is ~25s
        // one-time load + ~69s per warm query. BUT the minikube node is a docker
        // container throttled to 2 CPUs (NanoCpus=2e9), so in-pod it is several minutes
        // per query and will still time out here - the honest verdict for a 2-vCPU box
        // (raise it with `minikube start --cpus`). 180s captures the many-core case
        // (desktop, or a wider cluster) without waiting absurdly; a hung backend trips it.
        const PER_BACKEND: Duration = Duration::from_secs(180);
        let cfg = self.stored_llm_config().await;
        let caps = poseidon_ai::PlatformCaps::server();
        let item = poseidon_ai::TaggerInput {
            id: 0,
            title: "Login button unresponsive on mobile Safari after the latest deploy".to_string(),
            work_item_type: "Bug".to_string(),
            current_tags: vec![],
            description: None,
        };
        let allowed: Vec<String> = [
            "type:bug",
            "area:frontend",
            "priority:high",
            "platform:mobile",
            "needs:triage",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let mut set = tokio::task::JoinSet::new();
        for integ in cfg
            .integrations
            .iter()
            .filter(|i| i.kind != "webgpu")
            .cloned()
        {
            let item = item.clone();
            let allowed = allowed.clone();
            set.spawn(async move {
                let base =
                    |status: &str, ms: Option<u64>, tags: Vec<String>, error: Option<String>| {
                        serde_json::json!({
                            "id": integ.id, "name": integ.name, "kind": integ.kind,
                            "status": status, "ms": ms, "tags": tags, "error": error,
                        })
                    };
                if !integ.compatible(&caps) {
                    return base("unsupported", None, vec![], None);
                }
                if !integ.configured() {
                    return base("unconfigured", None, vec![], None);
                }
                let Some(tagger) = integ.build() else {
                    return base(
                        "error",
                        None,
                        vec![],
                        Some("backend failed to build".to_string()),
                    );
                };
                let start = Instant::now();
                let outcome = tokio::time::timeout(
                    PER_BACKEND,
                    tagger.suggest(&item, &allowed, &[], &Default::default(), ""),
                )
                .await;
                let ms = start.elapsed().as_millis() as u64;
                match outcome {
                    Err(_) => base("timeout", Some(ms), vec![], None),
                    Ok(Err(e)) => base("error", Some(ms), vec![], Some(e.to_string())),
                    Ok(Ok(tags)) => base(
                        "ok",
                        Some(ms),
                        tags.into_iter().map(|t| t.tag).collect(),
                        None,
                    ),
                }
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(v) = joined {
                results.push(v);
            }
        }
        // JoinSet completes out of order - restore the registry (priority) order.
        let order: std::collections::HashMap<&str, usize> = cfg
            .integrations
            .iter()
            .enumerate()
            .map(|(n, i)| (i.id.as_str(), n))
            .collect();
        results.sort_by_key(|r| {
            order
                .get(r["id"].as_str().unwrap_or(""))
                .copied()
                .unwrap_or(usize::MAX)
        });

        let webgpu: Vec<serde_json::Value> = cfg
            .integrations
            .iter()
            .filter(|i| i.kind == "webgpu")
            .map(|i| serde_json::json!({ "id": i.id, "name": i.name, "offline_model": i.offline_model }))
            .collect();
        serde_json::json!({ "results": results, "webgpu": webgpu })
    }

    /// The configured token verifier, if any (the HTTP `Scoped` extractor uses it).
    pub fn verifier(&self) -> Option<&crate::token::TokenVerifier> {
        self.verifier.as_ref().as_ref()
    }

    /// This owner's cached device-code token file (the native flow's replacement
    /// for the `az` token cache). One file per owner on the data volume, so hosted
    /// tenants never share tokens. Unlike the `az` cache, `default` gets a file too.
    fn token_path(&self) -> std::path::PathBuf {
        let dir = self.az_config_root.join(sanitize_owner(&self.owner));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("oauth-token.json")
    }

    /// Acquire an Azure DevOps access token from this owner's cached device-code
    /// session, silently refreshing when it's near expiry. `Err` (surfaced as "not
    /// signed in") when there's no usable session - the caller then falls back to a
    /// PAT or prompts a fresh sign-in.
    async fn acquire_token(&self, tenant: Option<&str>) -> Result<String, String> {
        acquire_cached_token(&self.token_path(), tenant).await
    }

    /// A view of this service scoped to a different owner - built per request
    /// from the auth header in a hosted (multi-tenant) deployment. Cheap: clones
    /// the Arc-backed handles and swaps the owner key.
    pub fn with_owner(&self, owner: &str) -> Self {
        let mut s = self.clone();
        s.owner = normalise_owner(owner);
        s
    }

    /// This service's owner key.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The configured poll cadence (instance-level).
    pub fn poll_interval(&self) -> Duration {
        self.config.instance().poll_interval_duration()
    }

    /// The config the Settings screen needs: instance `server` block plus this
    /// owner's teams, rules, and poll scope. No secret is ever here - PATs live
    /// only in the environment - so it's safe to serialise straight to the client.
    pub async fn config(&self) -> anyhow::Result<serde_json::Value> {
        let uc = self.config.user_config(&self.owner).await?;
        Ok(serde_json::json!({
            "server": self.config.instance(),
            "team": uc.teams,
            "rules": uc.rules,
            "poll_all_teams": uc.poll_all_teams,
        }))
    }

    /// Export this owner's full configuration as a YAML document (see
    /// [`ConfigBundle`]) - the unit of backup/restore, sharing, and declarative
    /// (CI/GitOps) config. Carries no secrets.
    pub async fn export_config(&self) -> anyhow::Result<String> {
        let config = self.config.user_config(&self.owner).await?;
        let reports = self.store.list_reports(&self.owner).await?;
        let bundle = ConfigBundle {
            poseidon: BundleMeta {
                schema: CONFIG_BUNDLE_SCHEMA,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                exported_at: Utc::now().to_rfc3339(),
                owner: Some(self.owner.clone()),
            },
            config,
            reports,
        };
        Ok(serde_norway::to_string(&bundle)?)
    }

    /// Import a YAML config document into this owner. `replace` overwrites the
    /// owner's config wholesale (declarative desired-state - the CI/GitOps path);
    /// otherwise merge, adding teams + reports not already present by name and
    /// keeping existing rules + poll scope. Never touches secrets. The Doctor
    /// reconciles imported teams' access checks on its next tick.
    pub async fn import_config(&self, yaml: &str, replace: bool) -> anyhow::Result<ImportSummary> {
        let bundle: ConfigBundle = serde_norway::from_str(yaml)
            .map_err(|e| anyhow::anyhow!("invalid config document: {e}"))?;
        if bundle.poseidon.schema > CONFIG_BUNDLE_SCHEMA {
            anyhow::bail!(
                "this config needs a newer POSEIDON (bundle schema {}, this build supports {})",
                bundle.poseidon.schema,
                CONFIG_BUNDLE_SCHEMA
            );
        }

        let summary = if replace {
            let s = ImportSummary {
                teams: bundle.config.teams.len(),
                reports: bundle.reports.len(),
                replaced: true,
            };
            self.config
                .set_user_config(&self.owner, bundle.config)
                .await?;
            self.store
                .replace_reports(&self.owner, &bundle.reports)
                .await?;
            s
        } else {
            let mut current = self.config.user_config(&self.owner).await?;
            let mut teams = 0;
            for t in bundle.config.teams {
                if !current
                    .teams
                    .iter()
                    .any(|x| x.name.eq_ignore_ascii_case(&t.name))
                {
                    current.teams.push(t);
                    teams += 1;
                }
            }
            self.config.set_user_config(&self.owner, current).await?;
            let existing: HashSet<String> = self
                .store
                .list_reports(&self.owner)
                .await?
                .into_iter()
                .map(|r| r.name)
                .collect();
            let mut reports = 0;
            for r in &bundle.reports {
                if !existing.contains(&r.name) {
                    self.store.upsert_report(&self.owner, r).await?;
                    reports += 1;
                }
            }
            ImportSummary {
                teams,
                reports,
                replaced: false,
            }
        };
        info!(
            owner = %self.owner,
            teams = summary.teams,
            reports = summary.reports,
            replaced = summary.replaced,
            "config imported"
        );
        Ok(summary)
    }

    /// Trusted import for **system / backend** callers (CLI, startup auto-import).
    /// Unlike [`import_config`] - which always writes `self.owner` (the
    /// authenticated caller) and ignores the bundle's `owner` field - this honors
    /// the target tenant: an explicit `owner_override`, else the bundle's `owner`,
    /// else [`DEFAULT_OWNER`]. Never call this from a public/authenticated HTTP
    /// path; that is what lets a bundle declare a tenant without letting one user
    /// overwrite another's config.
    pub async fn import_config_trusted(
        &self,
        yaml: &str,
        replace: bool,
        owner_override: Option<&str>,
    ) -> anyhow::Result<ImportSummary> {
        let owner = match owner_override {
            Some(o) => o.to_string(),
            None => {
                let bundle: ConfigBundle = serde_norway::from_str(yaml)
                    .map_err(|e| anyhow::anyhow!("invalid config document: {e}"))?;
                bundle
                    .poseidon
                    .owner
                    .unwrap_or_else(|| DEFAULT_OWNER.to_string())
            }
        };
        self.with_owner(&owner).import_config(yaml, replace).await
    }

    /// Record a frontend (webview) error into the same telemetry/log stream as
    /// the backend. Webview console errors are otherwise invisible to our logs,
    /// so the UI forwards uncaught errors here via a thin transport wrapper.
    pub fn log_client_error(&self, message: &str, stack: Option<&str>, url: Option<&str>) {
        tracing::error!(
            target: "poseidon_client",
            url = url.unwrap_or(""),
            stack = stack.unwrap_or(""),
            "client error: {message}"
        );
    }

    /// Add a team at runtime (from the UI) and persist. The Doctor's reconciler
    /// then registers its access check on the next tick. Returns whether it was
    /// newly added (`false` = a team with that name already existed).
    pub async fn add_team(&self, team: TeamConfig) -> anyhow::Result<bool> {
        self.config.add_team(&self.owner, team).await
    }

    /// Update an existing team (matched by `original` name) and persist.
    /// `false` if no team matched.
    pub async fn update_team(&self, original: &str, team: TeamConfig) -> anyhow::Result<bool> {
        self.config.update_team(&self.owner, original, team).await
    }

    /// Remove a team by name and persist. `false` if no such team.
    pub async fn remove_team(&self, name: &str) -> anyhow::Result<bool> {
        self.config.remove_team(&self.owner, name).await
    }

    /// Replace the owner's default hygiene ruleset (`[rules]`) and persist.
    pub async fn update_rules(&self, rules: poseidon_core::RuleSet) -> anyhow::Result<()> {
        info!("default rules updated");
        self.config.set_rules(&self.owner, rules).await
    }

    /// Set (`Some`) or clear (`None`) a team's `[team.rules]` override and
    /// persist. Clearing makes the team inherit the instance default again.
    /// `false` if no team matched.
    pub async fn update_team_rules(
        &self,
        team: &str,
        rules: Option<poseidon_core::RuleSet>,
    ) -> anyhow::Result<bool> {
        info!(team, override_set = rules.is_some(), "team rules updated");
        self.config.set_team_rules(&self.owner, team, rules).await
    }

    // ─────────────────────────── Poll ───────────────────────────

    /// The configured team names, in config order - powers the UI scope
    /// selector.
    pub async fn team_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .config
            .teams(&self.owner)
            .await?
            .iter()
            .map(|t| t.name.clone())
            .collect())
    }

    /// The teams a poll should fetch: all of them when `poll_all_teams` is on,
    /// else just the active team (the UI's selection, persisted in meta), falling
    /// back to the first configured team when none is set.
    async fn teams_to_poll(&self) -> Vec<TeamConfig> {
        let uc = match self.config.user_config(&self.owner).await {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let all = uc.teams;
        if uc.poll_all_teams {
            return all;
        }
        let active = self
            .store
            .get_meta(&self.owner, ACTIVE_TEAM_KEY)
            .await
            .ok()
            .flatten()
            .filter(|n| !n.is_empty());
        let chosen = active
            .and_then(|name| all.iter().find(|t| t.name == name).cloned())
            .or_else(|| all.first().cloned());
        chosen.into_iter().collect()
    }

    /// Remember which team the UI has selected so the active-team poll knows what
    /// to fetch. `None`/empty (the "All teams" view) clears it, so polls fall back
    /// to the first configured team.
    pub async fn set_active_team(&self, team: Option<&str>) -> anyhow::Result<()> {
        self.store
            .set_meta(&self.owner, ACTIVE_TEAM_KEY, team.unwrap_or(""))
            .await?;
        Ok(())
    }

    /// Toggle the poll-all-teams setting (persists to config).
    pub async fn set_poll_all_teams(&self, all: bool) -> anyhow::Result<()> {
        self.config.set_poll_all_teams(&self.owner, all).await
    }

    /// Sync a service catalog from `source` into this owner's `catalog` table (a
    /// wholesale replace, like a poll rewrites work items). Returns the number of
    /// repo-keyed rows stored. The source is injected so the orchestration is
    /// testable; the config-driven public entry that builds the source from the
    /// ruleset is layered on top of this.
    pub async fn sync_catalog_from(&self, source: &dyn CatalogSource) -> anyhow::Result<u64> {
        let entities = source.fetch().await?;
        let n = self.store.replace_catalog(&self.owner, &entities).await?;
        tracing::info!(
            owner = %self.owner, source = source.source_name(), rows = n,
            "service catalog synced"
        );
        Ok(n)
    }

    /// This owner's synced catalog rows (repo -> product/team), for status and for
    /// the tagging layer to resolve `product:*` from the catalog.
    pub async fn catalog(&self) -> anyhow::Result<Vec<CatalogEntity>> {
        Ok(self.store.catalog(&self.owner).await?)
    }

    /// Sync the service catalog from an uploaded CSV export (the config-driven CSV
    /// source). Uses the owner's `catalog.field_map` column mapping, or the Port
    /// "Service" export layout by default. Returns rows stored. The upload transport
    /// calls this; the `port`/`backstage` sources are stubbed behind the same trait.
    pub async fn sync_catalog_csv(&self, csv: &str) -> anyhow::Result<u64> {
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let map = cfg
            .rules
            .catalog
            .as_ref()
            .and_then(|c| c.field_map.as_ref())
            .map(field_map_from_config)
            .unwrap_or_default();
        let source = CsvCatalog::new(csv.to_string(), map);
        self.sync_catalog_from(&source).await
    }

    /// Poll the in-scope teams once: fetch work items, pipelines, and recent
    /// runs, and upsert them. Resolves each team's PAT from its configured
    /// environment variable - the ONLY place a secret is read, and it never
    /// leaves this function.
    #[tracing::instrument(skip_all, name = "poll_once")]
    pub async fn poll_once(&self) -> PollOutcome {
        let mut outcome = PollOutcome::default();
        let since = Utc::now() - chrono::Duration::days(HEALTH_WINDOW_DAYS);
        let teams = self.teams_to_poll().await;
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        tracing::debug!(teams = teams.len(), "poll cycle starting");

        for team in &teams {
            let provider = match self.build_team_provider(team).await {
                Ok(p) => p,
                Err(msg) => {
                    let m = format!("team \"{}\": {msg}", team.name);
                    warn!("{m}");
                    outcome.errors.push(m);
                    continue;
                }
            };

            match self.poll_team(&team.name, provider.as_ref(), since).await {
                Ok((wi, pl, runs, prs)) => {
                    outcome.teams_polled += 1;
                    outcome.work_items += wi;
                    outcome.pipelines += pl;
                    outcome.runs += runs;
                    outcome.pull_requests += prs;
                    // Backfill work-item origins for the "moved in from another board"
                    // source signal - only when this team uses it, capped per poll so
                    // the one-time backfill spreads across cycles (origins never change).
                    let rules = rules_for_team(&cfg, &team.name);
                    if rules
                        .moved_in_source
                        .as_deref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false)
                    {
                        let n = self
                            .backfill_origins(
                                &team.name,
                                provider.as_ref(),
                                ORIGIN_BACKFILL_PER_POLL,
                            )
                            .await;
                        if n > 0 {
                            info!(team = %team.name, fetched = n, "backfilled work-item origins");
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("team \"{}\": {e}", team.name);
                    warn!("{msg}");
                    outcome.errors.push(msg);
                }
            }
        }

        // Stamp the poll time only if at least one team succeeded, so the
        // dashboard's "last polled" reflects real data, not a no-op run.
        if outcome.teams_polled > 0 {
            let _ = self
                .store
                .set_meta(&self.owner, "last_polled_at", &Utc::now().to_rfc3339())
                .await;
        }
        info!(
            teams = outcome.teams_polled,
            work_items = outcome.work_items,
            pipelines = outcome.pipelines,
            runs = outcome.runs,
            pull_requests = outcome.pull_requests,
            errors = outcome.errors.len(),
            owner = %self.owner,
            "poll complete"
        );
        outcome
    }

    /// Fetch + store the origin area for up to `limit` items in `team` that don't
    /// have one yet (the "moved in" backfill). Records an empty string when the
    /// provider can't tell, so an item is never re-fetched. Returns how many it wrote.
    async fn backfill_origins(
        &self,
        team: &str,
        provider: &dyn poseidon_providers::Provider,
        limit: i64,
    ) -> usize {
        let ids = self
            .store
            .work_item_ids_missing_origin(&self.owner, Some(team), limit)
            .await
            .unwrap_or_default();
        if ids.is_empty() {
            return 0;
        }
        let mut got: Vec<(i64, String)> = Vec::with_capacity(ids.len());
        for id in ids {
            match provider.fetch_origin_area(id).await {
                Ok(area) => got.push((id, area.unwrap_or_default())),
                Err(e) => tracing::debug!(id, "origin fetch failed: {e}"),
            }
        }
        let n = got.len();
        if let Err(e) = self.store.set_origins(&self.owner, &got).await {
            warn!("store origins failed: {e}");
        }
        n
    }

    /// Poll every tenant once - each owner that has a config row (plus `default`
    /// as a floor). The background scheduler calls this so all users' boards stay
    /// fresh; a per-request manual refresh uses [`Service::poll_once`] on the
    /// already-scoped service. Standalone has one owner, so this is one pass.
    pub async fn poll_all_owners(&self) -> PollOutcome {
        let mut owners = self.store.list_config_owners().await.unwrap_or_default();
        if !owners.iter().any(|o| o == DEFAULT_OWNER) {
            owners.push(DEFAULT_OWNER.to_string());
        }
        let mut total = PollOutcome::default();
        for owner in owners {
            let out = self.with_owner(&owner).poll_once().await;
            total.teams_polled += out.teams_polled;
            total.work_items += out.work_items;
            total.pipelines += out.pipelines;
            total.runs += out.runs;
            total.pull_requests += out.pull_requests;
            total.errors.extend(out.errors);
        }
        total
    }

    /// Poll one provider and persist. Returns `(work_items, pipelines, runs,
    /// pull_requests)` counts. Any provider/store error propagates so the caller
    /// records it against that team.
    #[tracing::instrument(skip(self, provider, since), fields(team = %team_name))]
    async fn poll_team(
        &self,
        team_name: &str,
        provider: &dyn poseidon_providers::Provider,
        since: DateTime<Utc>,
    ) -> anyhow::Result<(usize, usize, usize, usize)> {
        let items = provider.fetch_work_items().await?;
        let pipelines = provider.fetch_pipelines().await?;
        let runs = provider.fetch_runs(since).await?;
        let pulls = provider.fetch_pull_requests().await?;
        tracing::debug!(
            work_items = items.len(),
            pipelines = pipelines.len(),
            runs = runs.len(),
            pull_requests = pulls.len(),
            "fetched from provider"
        );

        // Replace (not merge) each full-fetch set so entities that fell out of the
        // query scope - e.g. after enabling strict area path, or a deleted PR /
        // pipeline - are pruned, not left lingering on their screens. Safe here:
        // we only reach this line on a successful fetch. Runs are the exception:
        // `fetch_runs` is time-windowed history that feeds flow reports, so it
        // accumulates via upsert and is never pruned this way.
        self.store
            .replace_team_work_items(&self.owner, team_name, &items)
            .await?;
        self.store
            .replace_team_pipelines(&self.owner, team_name, &pipelines)
            .await?;
        self.store.upsert_runs(&self.owner, &runs).await?;
        self.store
            .replace_team_pull_requests(&self.owner, team_name, &pulls)
            .await?;

        Ok((items.len(), pipelines.len(), runs.len(), pulls.len()))
    }

    /// Resolve how a team authenticates, in priority order:
    /// 1. A Personal Access Token in the team's configured env var, if set.
    /// 2. Otherwise (single-tenant / standalone ONLY) a token brokered by the
    ///    Azure CLI (`az account get-access-token`) - the user's existing
    ///    `az login`, no PAT or app registration.
    ///
    /// **The `az` fallback is per-owner isolated.** The Azure CLI credential
    /// cache is normally `~/.azure` - a single machine-global store, unsafe to
    /// share across users in a hosted container (one `az login` clobbers another,
    /// a poll could mint a token as the WRONG user). We sidestep that by giving
    /// each hosted owner its own `AZURE_CONFIG_DIR` on the data volume (see
    /// [`Self::az_config_dir`]); the `default` (standalone) owner keeps using the
    /// machine `~/.azure`. So device-code sign-in works for every owner, each
    /// against its own token cache - no cross-tenant leak.
    ///
    /// Returns an error string (surfaced as a "not signed in" poll error and the
    /// UI's sign-in prompt) when no usable credential is available.
    async fn resolve_credential(&self, team: &TeamConfig) -> Result<Credential, String> {
        if let Ok(pat) = std::env::var(&team.auth.pat_env) {
            if !pat.trim().is_empty() {
                return Ok(Credential::Pat(pat));
            }
        }
        match self.acquire_token(team.tenant.as_deref()).await {
            Ok(token) => Ok(Credential::Bearer(token)),
            Err(e) => Err(format!(
                "not signed in - set ${} or sign in with Azure ({e})",
                team.auth.pat_env
            )),
        }
    }

    // ─────────────────────────── Doctor ───────────────────────────

    /// Build the Doctor from current config. The check set is:
    ///
    /// 1. a **reconciler** that keeps the registered access-check set in sync
    ///    with the configured teams (auto-registers missing, prunes orphaned) -
    ///    the self-healing "detect teams → ensure checks" mechanism;
    /// 2. one **per-team access check per registered key**, so the registry
    ///    (`doctor.checks`, persisted per owner in the DB) is the source of
    ///    truth - not derived on the fly.
    ///
    /// Rebuilt per call; cheap (checks are lightweight structs).
    async fn build_doctor(&self) -> Doctor {
        let teams = self.config.teams(&self.owner).await.unwrap_or_default();
        let registered = self
            .config
            .registered_checks(&self.owner)
            .await
            .unwrap_or_default();

        let mut checks: Vec<Arc<dyn Check>> = vec![
            Arc::new(TeamCheckReconciler::new(
                self.config.clone(),
                self.owner.clone(),
            )),
            Arc::new(UpdateCheck::new()),
        ];

        for key in &registered {
            if let Some(team_name) = key.strip_prefix("ado-access:") {
                if let Some(team) = teams.iter().find(|t| t.name == team_name) {
                    checks.push(Arc::new(AzureDevOpsAccessCheck::from_team(
                        team,
                        self.token_path(),
                    )));
                }
                // A key whose team no longer exists is skipped - the reconciler
                // prunes it on its next fix.
            }
        }
        Doctor::new(checks)
    }

    /// Run the health checks and report status (no fixes applied) - backs the
    /// traffic-light indicator + `GET /api/doctor`.
    pub async fn doctor_report(&self) -> DoctorReport {
        self.build_doctor()
            .await
            .report(Utc::now().to_rfc3339())
            .await
    }

    /// Run the checks AND apply auto-fixes (the reconciler auto-registers team
    /// checks; future checks may self-heal too). Driven by the background Doctor
    /// tick so the traffic light trends green without user action.
    pub async fn doctor_tick(&self) -> DoctorReport {
        self.build_doctor()
            .await
            .tick(Utc::now().to_rfc3339())
            .await
    }

    /// Run one check's server-side fix (the Doctor panel's Fix button, for
    /// non-interactive fixes). Interactive fixes (`fix_action`) are handled by
    /// the UI instead. `None` if the id is unknown.
    pub async fn doctor_fix(&self, id: &str) -> Option<FixResult> {
        self.build_doctor().await.fix(id).await
    }

    /// The tenant to sign into - the first configured team's, if any. Used by
    /// the desktop "Sign in" action so `az login` targets the org tenant.
    pub async fn primary_tenant(&self) -> Option<String> {
        self.config
            .teams(&self.owner)
            .await
            .unwrap_or_default()
            .iter()
            .find_map(|t| t.tenant.clone().filter(|s| !s.is_empty()))
    }

    /// Whether POSEIDON can authenticate right now, and how. A PAT on any team
    /// counts; otherwise it probes the Azure CLI once. Backs the UI's sign-in
    /// banner and the `/api/auth` endpoint.
    pub async fn auth_status(&self) -> AuthStatus {
        for team in &self.config.teams(&self.owner).await.unwrap_or_default() {
            let has_pat = std::env::var(&team.auth.pat_env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            if has_pat {
                return AuthStatus {
                    signed_in: true,
                    method: Some("pat".into()),
                    message: "Authenticated with a Personal Access Token.".into(),
                };
            }
        }
        // No PAT: probe this owner's cached device-code session (refreshing if
        // needed). A signed-in owner's token lives in their per-owner cache file.
        let tenant = self.primary_tenant().await;
        match self.acquire_token(tenant.as_deref()).await {
            Ok(_) => {
                info!(
                    tenant = tenant.as_deref().unwrap_or("<default>"),
                    "auth check: signed in via Azure device code"
                );
                AuthStatus {
                    signed_in: true,
                    method: Some("oauth".into()),
                    message: "Signed in with Azure (device code).".into(),
                }
            }
            Err(e) => {
                warn!(tenant = tenant.as_deref().unwrap_or("<default>"), error = %e, "auth check: not signed in");
                AuthStatus {
                    signed_in: false,
                    method: None,
                    message: format!("Not signed in - click Sign in to authenticate. ({e})"),
                }
            }
        }
    }

    /// Begin a **web** device-code sign-in for this owner (the hosted analogue of
    /// the desktop Tauri `sign_in` command). Spawns `az login --use-device-code`
    /// against the owner's isolated `AZURE_CONFIG_DIR`, waits (bounded) for the
    /// device-code prompt, and returns it so the browser can display "open <url>,
    /// enter <code>". The login then continues on a background task; its progress
    /// and outcome are recorded in [`Self::sign_in_status`].
    ///
    /// The desktop app keeps using its own Tauri flow (default owner, machine
    /// `~/.azure`); this path exists for owners reached over HTTP.
    pub async fn start_web_sign_in(&self) -> anyhow::Result<poseidon_providers::DeviceCode> {
        use poseidon_providers::oauth::{self, PollOutcome};
        let tenant = self.primary_tenant().await;
        let path = self.token_path();
        let http = reqwest::Client::new();

        // Native device-code: the prompt comes back immediately (no waiting on a
        // subprocess to print it), so the browser can show "open <url>, enter
        // <code>" at once; the grant then completes on a background poll.
        let start = oauth::start_device_code(&http, tenant.as_deref()).await?;
        let prompt = poseidon_providers::DeviceCode {
            url: start.verification_uri.clone(),
            code: start.user_code.clone(),
        };
        self.signins.lock().unwrap().insert(
            self.owner.clone(),
            SigninState::Pending {
                url: prompt.url.clone(),
                code: prompt.code.clone(),
            },
        );

        let signins = self.signins.clone();
        let owner = self.owner.clone();
        let device_code = start.device_code;
        let mut wait = start.interval.max(1);
        let expires_in = start.expires_in.max(60);
        tokio::spawn(async move {
            let deadline = Utc::now() + chrono::Duration::seconds(expires_in as i64);
            let state = loop {
                if Utc::now() >= deadline {
                    break SigninState::Failed {
                        error: "sign-in timed out".to_string(),
                    };
                }
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                match oauth::poll_once(&http, tenant.as_deref(), &device_code).await {
                    Ok(PollOutcome::Pending) => continue,
                    Ok(PollOutcome::SlowDown) => {
                        wait += 5;
                        continue;
                    }
                    Ok(PollOutcome::Ready(ts)) => {
                        write_cached_token(&path, &CachedToken::from(ts));
                        info!(owner = %owner, "web sign-in completed");
                        break SigninState::Done;
                    }
                    Ok(PollOutcome::Failed(msg)) => break SigninState::Failed { error: msg },
                    Err(e) => {
                        break SigninState::Failed {
                            error: e.to_string(),
                        }
                    }
                }
            };
            signins.lock().unwrap().insert(owner, state);
        });

        Ok(prompt)
    }

    /// The current web device-code sign-in state for this owner (Idle if none was
    /// started this process). Polled by the browser after `start_web_sign_in`.
    pub fn sign_in_status(&self) -> SigninState {
        self.signins
            .lock()
            .unwrap()
            .get(&self.owner)
            .cloned()
            .unwrap_or(SigninState::Idle)
    }

    // ─────────────────────────── Reads ───────────────────────────
    //
    // Every read takes an optional `team` scope. `None` = all teams (the "All
    // teams" roll-up); `Some(name)` narrows to one team. The scope threads
    // straight through to the store's `team = COALESCE(?, team)` filter.

    /// Stored work items, optionally scoped to one team, with each item's linked
    /// pull requests resolved (id + status + url) from the polled PR set.
    pub async fn work_items(&self, team: Option<&str>) -> anyhow::Result<Vec<WorkItem>> {
        let mut items = self.store.list_work_items(&self.owner, team).await?;
        self.attach_linked_prs(&mut items, team).await?;
        // Advisory tag suggestions: each team's keyword map, plus any stored AI
        // suggestions (both dropped if already applied; AI de-duped vs keyword).
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let use_desc = self.tag_use_description().await;
        let ai = self
            .store
            .ai_suggestions(&self.owner, team)
            .await
            .unwrap_or_default();
        // For the "moved in from another board" source signal: each item's origin
        // area (where it was created) + each team's own area path, so we can tell a
        // moved-in item from a natively-created one.
        let origins = self
            .store
            .origins(&self.owner, team)
            .await
            .unwrap_or_default();
        let area_of: std::collections::HashMap<String, String> = cfg
            .teams
            .iter()
            .filter_map(|t| t.area_path.clone().map(|a| (t.name.clone(), a)))
            .collect();
        // Catalog-derived repo -> product:* map (owner-wide; the portal is per-tenant,
        // so the aliases live on the owner's default ruleset). Built once. Empty when
        // no catalog is synced, in which case the config `repo_tags` carry product on
        // their own exactly as before.
        let catalog_repo_product: std::collections::BTreeMap<String, String> = {
            let entities = self.store.catalog(&self.owner).await.unwrap_or_default();
            if entities.is_empty() {
                Default::default()
            } else {
                let aliases: std::collections::HashMap<String, String> = cfg
                    .rules
                    .catalog
                    .as_ref()
                    .map(|c| c.product_aliases.clone().into_iter().collect())
                    .unwrap_or_default();
                poseidon_core::repo_product_map(&entities, &aliases)
            }
        };
        // For parent-tag inheritance: a snapshot of every item's applied tags keyed
        // by id, taken before the mutation loop so a child can read its parent's
        // (applied, not suggested) product:/area: tags without a borrow conflict.
        let tags_by_id: std::collections::HashMap<i64, Vec<String>> =
            items.iter().map(|it| (it.id, it.tags.clone())).collect();
        for it in &mut items {
            let rules = rules_for_team(&cfg, &it.team);
            let mut suggestions = poseidon_rules::suggest_tags(it, rules, use_desc);
            // Moved in from another board -> suggest the configured source (e.g.
            // source:sre): the item was created outside this team's area path and
            // later moved in. Deterministic; the signal isn't in the body.
            if let Some(src) = rules
                .moved_in_source
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let team_area = area_of.get(&it.team).map(|s| s.as_str()).unwrap_or("");
                let origin = origins.get(&it.id).map(|s| s.as_str()).unwrap_or("");
                let moved_in = !team_area.is_empty()
                    && !origin.is_empty()
                    && !origin.to_lowercase().starts_with(&team_area.to_lowercase());
                let have_src = it.tags.iter().any(|t| t.eq_ignore_ascii_case(src))
                    || suggestions.iter().any(|s| s.tag.eq_ignore_ascii_case(src));
                if moved_in && !have_src {
                    suggestions.push(poseidon_core::TagSuggestion {
                        tag: src.to_string(),
                        reasons: vec![format!("moved in from \"{origin}\" - not created here")],
                        replaces: None,
                    });
                }
            }
            // Parent inheritance: a child's product/area is almost always its
            // parent's - inherit the parent's APPLIED product:/area: tags as
            // suggestions (never source:; how work arrived is per-item). The
            // strongest signal for the many thin child tasks whose Epic/Feature
            // already defines what they're about.
            if rules.inherit_parent_tags {
                if let Some((pid, ptags)) = it
                    .parent_id
                    .and_then(|pid| tags_by_id.get(&pid).map(|t| (pid, t)))
                {
                    for pt in ptags {
                        if !(pt.starts_with("product:") || pt.starts_with("area:")) {
                            continue;
                        }
                        let key = pt.to_lowercase();
                        let have = it.tags.iter().any(|t| t.to_lowercase() == key)
                            || suggestions.iter().any(|s| s.tag.to_lowercase() == key);
                        if !have {
                            suggestions.push(poseidon_core::TagSuggestion {
                                tag: pt.clone(),
                                reasons: vec![format!("inherited from parent #{pid}")],
                                replaces: None,
                            });
                        }
                    }
                }
            }
            // Linked-repo -> tag: a PR to "PlatformDeployment" implies its area/product
            // more reliably than any keyword. Match each rule's keywords (repo names,
            // whole-word) against the item's linked repos.
            if !it.linked_repos.is_empty() {
                for rule in &rules.repo_tags {
                    let hit = rule.keywords.iter().find(|kw| {
                        it.linked_repos
                            .iter()
                            .any(|repo| poseidon_rules::contains_word(repo, kw))
                    });
                    if let Some(kw) = hit {
                        let key = rule.tag.to_lowercase();
                        let have = it.tags.iter().any(|t| t.to_lowercase() == key)
                            || suggestions.iter().any(|s| s.tag.to_lowercase() == key);
                        if !have {
                            suggestions.push(poseidon_core::TagSuggestion {
                                tag: rule.tag.clone(),
                                reasons: vec![format!("linked repo matches \"{kw}\"")],
                                replaces: None,
                            });
                        }
                    }
                }
            }
            // Catalog-derived product: an exact linked-repo -> product:* match from the
            // synced service catalog. The config `repo_tags` above are the manual
            // OVERRIDE, so the catalog only speaks when the item STILL has no product on
            // any axis (applied or already suggested) - a curated mapping always wins,
            // and one product per item.
            if !catalog_repo_product.is_empty() && !it.linked_repos.is_empty() {
                let has_product = it
                    .tags
                    .iter()
                    .any(|t| t.to_lowercase().starts_with("product:"))
                    || suggestions
                        .iter()
                        .any(|s| s.tag.to_lowercase().starts_with("product:"));
                if !has_product {
                    if let Some((repo, tag)) = it
                        .linked_repos
                        .iter()
                        .find_map(|r| catalog_repo_product.get(r).map(|t| (r, t)))
                    {
                        suggestions.push(poseidon_core::TagSuggestion {
                            tag: tag.clone(),
                            reasons: vec![format!("catalog: repo \"{repo}\" is {tag}")],
                            replaces: None,
                        });
                    }
                }
            }
            // Stored AI suggestions are a CACHE, not truth. Never surface them for an
            // item that is now underspecified: those are exactly the from-nothing
            // guesses (`area:ssa` on an empty body) this classification suppresses, so
            // showing a stale one would contradict the "refine first" signal. The
            // deterministic keyword/refine suggestions still apply. (The next AI run
            // prunes the stale rows from the store; see generate_tag_suggestions_with.)
            let stale_ai = poseidon_rules::is_underspecified(it, rules);
            if let Some(extra) = ai.get(&it.id).filter(|_| !stale_ai) {
                let applied: std::collections::HashSet<String> =
                    it.tags.iter().map(|t| t.to_lowercase()).collect();
                let mut have: std::collections::HashSet<String> =
                    suggestions.iter().map(|s| s.tag.to_lowercase()).collect();
                for (tag, reason) in extra {
                    let key = tag.to_lowercase();
                    if applied.contains(&key) || !have.insert(key) {
                        continue;
                    }
                    suggestions.push(poseidon_core::TagSuggestion {
                        tag: tag.clone(),
                        reasons: vec![reason.clone()],
                        replaces: None,
                    });
                }
            }
            it.tag_suggestions = suggestions;
        }
        Ok(items)
    }

    /// Run the AI tagger over the owner's work items (optionally one team),
    /// proposing canonical tags from each team's approved set and STORING them as
    /// advisory suggestions - never applied; a person clicks to apply. Returns a
    /// summary. Sequential model calls: fine for a team-sized batch; a very large
    /// backlog wants a background job (see BACKLOG). A per-item error is logged +
    /// skipped so one bad item never fails the whole run.
    pub async fn generate_tag_suggestions(
        &self,
        team: Option<&str>,
    ) -> anyhow::Result<AiSuggestSummary> {
        self.generate_tag_suggestions_with(team, None, |_, _, _| {})
            .await
    }

    /// As [`Self::generate_tag_suggestions`], but scoped to `ids` when given (a
    /// subset of THIS owner's items - the UI passes the selected rows, so a run only
    /// ever touches what the user ticked), and calling `progress(done, total,
    /// suggestions)` after each item so the background job (below) can publish a live
    /// count. `done`/`total` count all considered items (skipped ones included) so
    /// the caller gets a monotonic progress bar.
    pub async fn generate_tag_suggestions_with<F: FnMut(usize, usize, usize)>(
        &self,
        team: Option<&str>,
        ids: Option<&[i64]>,
        mut progress: F,
    ) -> anyhow::Result<AiSuggestSummary> {
        let ai = self.ai_tagger().await.ok_or_else(|| {
            anyhow::anyhow!("AI tagging is not configured - enable it in Settings")
        })?;
        let mut items = self.store.list_work_items(&self.owner, team).await?;
        // Selection scope: keep only the requested ids (still from the owner's own
        // items, so no cross-tenant reach). None = the whole team scope (CLI).
        if let Some(ids) = ids {
            let want: std::collections::HashSet<i64> = ids.iter().copied().collect();
            items.retain(|it| want.contains(&it.id));
        }
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let use_desc = self.tag_use_description().await;
        let total = items.len();

        // Concrete tag values actually in use, grouped by team, so wildcard slots
        // (`area:*`/`source:*`) can be filled from real values already on the
        // backlog rather than invented fresh. Harvested from the full scope once.
        let mut observed: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for it in &items {
            observed
                .entry(it.team.clone())
                .or_default()
                .extend(it.tags.iter().cloned());
        }
        let no_tags: Vec<String> = Vec::new();

        let mut summary = AiSuggestSummary::default();
        for (i, it) in items.iter().enumerate() {
            let rules = rules_for_team(&cfg, &it.team);
            // Too little body to tag from: the deterministic suggester flags it "to
            // refine"; don't let the model confabulate an area from nothing. Also PRUNE
            // any stale AI suggestions left from a previous run (before this item lost
            // its body / before the rule existed) so they stop being re-shown.
            if poseidon_rules::is_underspecified(it, rules) {
                self.store
                    .set_ai_suggestions(&self.owner, &it.team, it.id, &[])
                    .await?;
                progress(i + 1, total, summary.suggestions);
                continue;
            }
            let allowed = candidate_tags(rules, observed.get(&it.team).unwrap_or(&no_tags));
            if !allowed.is_empty() {
                summary.considered += 1;
                let mut input = poseidon_ai::TaggerInput::from(it);
                if !use_desc {
                    input.description = None; // owner opted out of sending bodies to the tagger
                }
                match ai
                    .suggest(
                        &input,
                        &allowed,
                        &rules.required_tags,
                        &tag_hints(rules),
                        rules.team_background.as_deref().unwrap_or(""),
                    )
                    .await
                {
                    Ok(sugg) => {
                        // Cap to the team's configured ceiling (or the adaptive default
                        // that scales with required axes). Required picks are listed
                        // first by the prompt, so truncation keeps them and trims only
                        // the optional extras.
                        let cap = rules.max_suggestions.unwrap_or_else(|| {
                            poseidon_ai::default_max_suggestions(rules.required_tags.len())
                        });
                        let pairs: Vec<(String, String)> = sugg
                            .iter()
                            .take(cap)
                            .map(|s| {
                                (
                                    s.tag.clone(),
                                    s.reasons.first().cloned().unwrap_or_default(),
                                )
                            })
                            .collect();
                        self.store
                            .set_ai_suggestions(&self.owner, &it.team, it.id, &pairs)
                            .await?;
                        if !pairs.is_empty() {
                            summary.with_suggestions += 1;
                            summary.suggestions += pairs.len();
                        }
                    }
                    Err(e) => tracing::warn!(item = it.id, "AI tag suggestion failed: {e}"),
                }
            }
            progress(i + 1, total, summary.suggestions);
        }
        info!(
            team = team.unwrap_or("all"),
            considered = summary.considered,
            suggestions = summary.suggestions,
            "AI tag-suggestion run complete"
        );
        Ok(summary)
    }

    /// Start a tag-suggestion run in the BACKGROUND for this owner and return at
    /// once; the browser polls [`Self::tag_suggestions_status`]. Model inference is
    /// slow, so running it inside the request 504s over a big backlog. Idempotent
    /// per owner: if a run is already in flight it's left alone.
    pub async fn start_tag_suggestions(
        &self,
        team: Option<String>,
        ids: Option<Vec<i64>>,
    ) -> anyhow::Result<()> {
        if !self.ai_enabled().await {
            anyhow::bail!("AI tagging is not configured - enable it in Settings");
        }
        {
            let mut jobs = self.suggest_jobs.lock().unwrap();
            if matches!(jobs.get(&self.owner), Some(SuggestState::Running { .. })) {
                return Ok(()); // already running for this owner
            }
            jobs.insert(
                self.owner.clone(),
                SuggestState::Running {
                    done: 0,
                    total: 0,
                    suggestions: 0,
                },
            );
        }
        let svc = self.clone();
        let jobs = self.suggest_jobs.clone();
        let owner = self.owner.clone();
        tokio::spawn(async move {
            let (pjobs, powner) = (jobs.clone(), owner.clone());
            let result = svc
                .generate_tag_suggestions_with(
                    team.as_deref(),
                    ids.as_deref(),
                    move |done, total, suggestions| {
                        pjobs.lock().unwrap().insert(
                            powner.clone(),
                            SuggestState::Running {
                                done,
                                total,
                                suggestions,
                            },
                        );
                    },
                )
                .await;
            let final_state = match result {
                Ok(summary) => SuggestState::Done { summary },
                Err(e) => SuggestState::Failed {
                    error: e.to_string(),
                },
            };
            jobs.lock().unwrap().insert(owner, final_state);
        });
        Ok(())
    }

    /// The current tag-suggestion run state for this owner (Idle if none started
    /// this process). Polled by the browser after `start_tag_suggestions`.
    pub fn tag_suggestions_status(&self) -> SuggestState {
        self.suggest_jobs
            .lock()
            .unwrap()
            .get(&self.owner)
            .cloned()
            .unwrap_or(SuggestState::Idle)
    }

    /// Store suggestions computed in the BROWSER (the WebGPU path: the model runs
    /// client-side, then posts results here). This is a trust boundary - we RE-VALIDATE
    /// every tag against the item's team canonical set server-side (the browser can't
    /// inject arbitrary tags), dedupe, and cap per item, before storing. Owner-scoped.
    pub async fn store_tag_suggestions(
        &self,
        team: Option<&str>,
        incoming: Vec<BrowserSuggestion>,
    ) -> anyhow::Result<AiSuggestSummary> {
        let items = self.store.list_work_items(&self.owner, team).await?;
        let team_of: HashMap<i64, String> = items.iter().map(|i| (i.id, i.team.clone())).collect();
        // Same wildcard-slot expansion as the server-side run: gather the concrete
        // tags in use per team so a browser-computed `area:mobile` re-validates
        // against the approved set instead of being dropped as a hallucination.
        let mut observed: HashMap<String, Vec<String>> = HashMap::new();
        for it in &items {
            observed
                .entry(it.team.clone())
                .or_default()
                .extend(it.tags.iter().cloned());
        }
        let no_tags: Vec<String> = Vec::new();
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let mut summary = AiSuggestSummary::default();
        for entry in incoming {
            let Some(team_name) = team_of.get(&entry.id) else {
                continue;
            };
            summary.considered += 1;
            let rules = rules_for_team(&cfg, team_name);
            let canon: HashMap<String, String> =
                candidate_tags(rules, observed.get(team_name).unwrap_or(&no_tags))
                    .into_iter()
                    .map(|t| (t.to_lowercase(), t))
                    .collect();
            let mut seen = std::collections::HashSet::new();
            let mut pairs: Vec<(String, String)> = Vec::new();
            for t in entry.tags {
                if pairs.len() >= 3 {
                    break;
                }
                let key = t.tag.trim().to_lowercase();
                if let Some(cn) = canon.get(&key) {
                    if seen.insert(key) {
                        pairs.push((cn.clone(), t.reason));
                    }
                }
            }
            self.store
                .set_ai_suggestions(&self.owner, team_name, entry.id, &pairs)
                .await?;
            if !pairs.is_empty() {
                summary.with_suggestions += 1;
                summary.suggestions += pairs.len();
            }
        }
        info!(
            team = team.unwrap_or("all"),
            considered = summary.considered,
            suggestions = summary.suggestions,
            "browser (WebGPU) suggestions stored"
        );
        Ok(summary)
    }

    // ───────────────────────────── AI healthcheck audit ─────────────────────
    //
    // On-demand, scoped, advisory. A person runs it over the rows they picked; the
    // active AI backend judges each item's data quality; the findings are stored and
    // surfaced as `ai_audit` flags until the next run. Server path = a background job
    // (this process runs the model); browser path = the server hands out prompts, the
    // browser's WebGPU model runs them, and the replies are posted back to be parsed
    // + stored here (the same value-or-prompt split as field drafting).

    /// The items an audit run should judge for `team`, optionally narrowed to `ids`
    /// (the rows the user ticked). Owner-scoped, so a selection can never reach
    /// another tenant's items.
    async fn audit_scope(
        &self,
        team: Option<&str>,
        ids: Option<&[i64]>,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let mut items = self.store.list_work_items(&self.owner, team).await?;
        if let Some(ids) = ids {
            let want: std::collections::HashSet<i64> = ids.iter().copied().collect();
            items.retain(|it| want.contains(&it.id));
        }
        Ok(items)
    }

    /// The team background glossary for an item's team (so the model doesn't mistake
    /// correct internal jargon for nonsense). Read from the item's team-effective rules.
    fn audit_background(cfg: &poseidon_core::UserConfig, team: &str) -> String {
        rules_for_team(cfg, team)
            .team_background
            .clone()
            .unwrap_or_default()
    }

    /// Run the healthcheck audit on the server (the active backend runs in THIS
    /// process), scoped to `team`/`ids`, calling `progress(done, total, findings)`
    /// after each item so the background job can publish a live count. Requires a
    /// server-side backend; the browser path uses [`Self::audit_prompts`] instead.
    pub async fn run_healthcheck_audit_with<F: FnMut(usize, usize, usize)>(
        &self,
        team: Option<&str>,
        ids: Option<&[i64]>,
        mut progress: F,
    ) -> anyhow::Result<AiAuditSummary> {
        let ai = self.ai_tagger().await.ok_or_else(|| {
            anyhow::anyhow!("The AI healthcheck needs a server-side model - enable it in Settings")
        })?;
        let items = self.audit_scope(team, ids).await?;
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let total = items.len();
        let mut summary = AiAuditSummary::default();
        for (i, it) in items.iter().enumerate() {
            summary.considered += 1;
            let input = poseidon_ai::AuditInput::from(it);
            let background = Self::audit_background(&cfg, &it.team);
            match ai.audit_item(&input, &background).await {
                Ok(issues) => {
                    let pairs: Vec<(String, String)> = issues
                        .iter()
                        .map(|f| (f.kind.as_str().to_string(), f.detail.clone()))
                        .collect();
                    self.store
                        .set_ai_audit(&self.owner, &it.team, it.id, &pairs)
                        .await?;
                    if !pairs.is_empty() {
                        summary.flagged += 1;
                        summary.findings += pairs.len();
                    }
                }
                Err(e) => tracing::warn!(item = it.id, "AI healthcheck audit failed: {e}"),
            }
            progress(i + 1, total, summary.findings);
        }
        info!(
            team = team.unwrap_or("all"),
            considered = summary.considered,
            findings = summary.findings,
            "AI healthcheck audit run complete"
        );
        Ok(summary)
    }

    /// Start a healthcheck audit in the BACKGROUND for this owner and return at once;
    /// the browser polls [`Self::healthcheck_audit_status`]. Idempotent per owner: a
    /// run already in flight is left alone. Server-backend path only.
    pub async fn start_healthcheck_audit(
        &self,
        team: Option<String>,
        ids: Option<Vec<i64>>,
    ) -> anyhow::Result<()> {
        if !self.ai_enabled().await {
            anyhow::bail!("The AI healthcheck needs a server-side model - enable it in Settings");
        }
        {
            let mut jobs = self.audit_jobs.lock().unwrap();
            if matches!(jobs.get(&self.owner), Some(AuditState::Running { .. })) {
                return Ok(()); // already running for this owner
            }
            jobs.insert(
                self.owner.clone(),
                AuditState::Running {
                    done: 0,
                    total: 0,
                    findings: 0,
                },
            );
        }
        let svc = self.clone();
        let jobs = self.audit_jobs.clone();
        let owner = self.owner.clone();
        tokio::spawn(async move {
            let (pjobs, powner) = (jobs.clone(), owner.clone());
            let result = svc
                .run_healthcheck_audit_with(
                    team.as_deref(),
                    ids.as_deref(),
                    move |done, total, findings| {
                        pjobs.lock().unwrap().insert(
                            powner.clone(),
                            AuditState::Running {
                                done,
                                total,
                                findings,
                            },
                        );
                    },
                )
                .await;
            let final_state = match result {
                Ok(summary) => AuditState::Done { summary },
                Err(e) => AuditState::Failed {
                    error: e.to_string(),
                },
            };
            jobs.lock().unwrap().insert(owner, final_state);
        });
        Ok(())
    }

    /// The current healthcheck audit run state for this owner (Idle if none started
    /// this process). Polled by the browser after `start_healthcheck_audit`.
    pub fn healthcheck_audit_status(&self) -> AuditState {
        self.audit_jobs
            .lock()
            .unwrap()
            .get(&self.owner)
            .cloned()
            .unwrap_or(AuditState::Idle)
    }

    /// Build the per-item audit prompts for the BROWSER (WebGPU) path: the server
    /// assembles the exact system + user messages so the browser's local model runs
    /// the same prompt, then posts replies to [`Self::store_healthcheck_audit`].
    /// Owner-scoped via [`Self::audit_scope`].
    pub async fn audit_prompts(
        &self,
        team: Option<&str>,
        ids: Option<&[i64]>,
    ) -> anyhow::Result<Vec<AuditPrompt>> {
        let items = self.audit_scope(team, ids).await?;
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        Ok(items
            .iter()
            .map(|it| {
                let input = poseidon_ai::AuditInput::from(it);
                let background = Self::audit_background(&cfg, &it.team);
                AuditPrompt {
                    id: it.id,
                    system: poseidon_ai::AUDIT_SYSTEM_PROMPT.to_string(),
                    user: poseidon_ai::build_audit_prompt(&input, &background),
                }
            })
            .collect())
    }

    /// Store browser-computed (WebGPU) audit replies. The trust boundary for the
    /// browser path: every reply is RE-PARSED server-side via
    /// [`poseidon_ai::parse_audit_response`] (the browser can't inject arbitrary
    /// findings), then stored against the item's own team. Owner-scoped.
    pub async fn store_healthcheck_audit(
        &self,
        team: Option<&str>,
        incoming: Vec<BrowserAuditResult>,
    ) -> anyhow::Result<AiAuditSummary> {
        let items = self.audit_scope(team, None).await?;
        let team_of: HashMap<i64, String> = items.iter().map(|i| (i.id, i.team.clone())).collect();
        let mut summary = AiAuditSummary::default();
        for entry in incoming {
            let Some(team_name) = team_of.get(&entry.id) else {
                continue; // not this owner's item (or out of scope) - ignore
            };
            summary.considered += 1;
            let issues = poseidon_ai::parse_audit_response(&entry.text);
            let pairs: Vec<(String, String)> = issues
                .iter()
                .map(|f| (f.kind.as_str().to_string(), f.detail.clone()))
                .collect();
            self.store
                .set_ai_audit(&self.owner, team_name, entry.id, &pairs)
                .await?;
            if !pairs.is_empty() {
                summary.flagged += 1;
                summary.findings += pairs.len();
            }
        }
        info!(
            team = team.unwrap_or("all"),
            considered = summary.considered,
            findings = summary.findings,
            "browser (WebGPU) healthcheck audit stored"
        );
        Ok(summary)
    }

    /// Run the on-demand near-duplicate scan over a team's active items (all teams when
    /// `team` is `None`), storing the results as `near_duplicate` flags until the next
    /// scan. Deterministic (TF-IDF cosine, no model) so it runs inline. Each item judged
    /// by ITS team's threshold; the scan is partitioned per team so cross-team titles
    /// aren't compared (different backlogs, different vocab).
    pub async fn run_duplicate_scan(&self, team: Option<&str>) -> anyhow::Result<DupScanSummary> {
        let items = self.store.list_work_items(&self.owner, team).await?;
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        // Partition by team so each group is scanned against its own ruleset/threshold.
        let mut groups: std::collections::BTreeMap<&str, Vec<WorkItem>> =
            std::collections::BTreeMap::new();
        for it in &items {
            groups.entry(it.team.as_str()).or_default().push(it.clone());
        }
        let mut rows: Vec<(i64, String, String)> = Vec::new();
        let mut pairs = 0usize;
        for (team_name, group) in &groups {
            let rules = rules_for_team(&cfg, team_name);
            for nd in poseidon_rules::find_near_duplicates(group, rules) {
                let listed: Vec<String> = nd
                    .matches
                    .iter()
                    .map(|(id, score)| format!("#{} ({}%)", id, (score * 100.0).round() as i64))
                    .collect();
                pairs += nd.matches.len();
                rows.push((
                    nd.id,
                    team_name.to_string(),
                    format!("resembles {}", listed.join(", ")),
                ));
            }
        }
        let flagged = rows.len();
        self.store
            .replace_near_duplicates(&self.owner, team, &rows)
            .await?;
        info!(
            team = team.unwrap_or("all"),
            scanned = items.len(),
            flagged,
            "near-duplicate scan complete"
        );
        Ok(DupScanSummary {
            scanned: items.len(),
            flagged,
            pairs,
        })
    }

    /// Populate each item's `linked_prs` (the coloured chips) from its stored
    /// `linked_pr_ids`, resolving status/url against the polled PR set. Abandoned
    /// links are hidden unless the team's rule opts them in; a linked PR we never
    /// polled shows as "unknown". Shared by the list view and single-item writes.
    async fn attach_linked_prs(
        &self,
        items: &mut [WorkItem],
        team: Option<&str>,
    ) -> anyhow::Result<()> {
        let prs = self.store.list_pull_requests(&self.owner, team).await?;
        let pr_map: HashMap<i64, &PullRequest> = prs.iter().map(|p| (p.id, p)).collect();
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        for it in items.iter_mut() {
            if it.linked_pr_ids.is_empty() {
                it.linked_prs = Vec::new();
                continue;
            }
            let include_abandoned = rules_for_team(&cfg, &it.team)
                .pull_requests
                .link_include_abandoned;
            it.linked_prs = it
                .linked_pr_ids
                .iter()
                .filter_map(|id| match pr_map.get(id) {
                    Some(p) => {
                        if p.status == PrStatus::Abandoned && !include_abandoned {
                            None
                        } else {
                            Some(LinkedPr {
                                id: *id,
                                status: p.status,
                                is_draft: p.is_draft,
                                url: p.url.clone(),
                            })
                        }
                    }
                    // Linked PR not in our polled set - show it, status unknown.
                    None => Some(LinkedPr {
                        id: *id,
                        status: PrStatus::Unknown,
                        is_draft: false,
                        url: String::new(),
                    }),
                })
                .collect();
        }
        Ok(())
    }

    /// Build a provider for `team` with a freshly-resolved credential. Shared by
    /// every write/lookup path that needs to talk to the provider directly.
    async fn provider_for(
        &self,
        team: &str,
    ) -> anyhow::Result<Box<dyn poseidon_providers::Provider>> {
        let team_cfg = self
            .config
            .teams(&self.owner)
            .await?
            .into_iter()
            .find(|t| t.name == team)
            .ok_or_else(|| anyhow::anyhow!("unknown team \"{team}\""))?;
        self.build_team_provider(&team_cfg)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Build the provider for a team, resolving its credential - except the stub
    /// provider, which is offline and needs none (so it works in an auth-less
    /// container). The single place poll and on-demand reads agree on how a
    /// team's provider is constructed.
    async fn build_team_provider(
        &self,
        team_cfg: &TeamConfig,
    ) -> Result<Box<dyn poseidon_providers::Provider>, String> {
        use poseidon_core::ProviderKind;
        let credential = match team_cfg.provider {
            // Offline, needs no credential (works in an auth-less container).
            ProviderKind::Stub => poseidon_providers::Credential::Pat(String::new()),
            // GitHub / GitLab read public repos ANONYMOUSLY: a token (from the
            // team's configured env var) is optional and only raises rate limits
            // or reaches private repos. Absent = empty credential = anonymous.
            // No Azure sign-in fallback for these providers.
            ProviderKind::GitHub | ProviderKind::GitLab => {
                let token = std::env::var(&team_cfg.auth.pat_env).unwrap_or_default();
                poseidon_providers::Credential::Pat(token)
            }
            // Azure DevOps: PAT env if set, else an interactive/az token.
            ProviderKind::AzureDevOps => self.resolve_credential(team_cfg).await?,
        };
        poseidon_providers::build_provider(team_cfg, credential).map_err(|e| e.to_string())
    }

    /// Resolve a single PR live by id (for a linked-PR chip that fell outside the
    /// polled window, so its chip can both open in the browser and take on its
    /// real status colour). Returns the normalised PR.
    pub async fn resolve_pull_request(
        &self,
        team: &str,
        pr_id: i64,
    ) -> anyhow::Result<PullRequest> {
        let provider = self.provider_for(team).await?;
        Ok(provider.fetch_pull_request(pr_id).await?)
    }

    /// Add or remove a work-item <-> PR link, writing the ADO artifact-link
    /// relation through the provider, persisting the item's post-write view, and
    /// returning it with fresh chips + hygiene flags (same shape as an edit).
    #[tracing::instrument(skip(self), fields(team = %team, work_item_id, pr_id, link))]
    pub async fn link_work_item_pr(
        &self,
        team: &str,
        work_item_id: i64,
        pr_id: i64,
        link: bool,
    ) -> anyhow::Result<(WorkItem, Vec<Flag>)> {
        let provider = self.provider_for(team).await?;
        let mut updated = if link {
            provider.link_pr(work_item_id, pr_id).await?
        } else {
            provider.unlink_pr(work_item_id, pr_id).await?
        };
        self.store
            .upsert_work_items(&self.owner, std::slice::from_ref(&updated))
            .await?;
        let flags = self.evaluate_scoped(std::slice::from_ref(&updated)).await;
        self.attach_linked_prs(std::slice::from_mut(&mut updated), Some(team))
            .await?;
        info!(
            link,
            flags = flags.len(),
            "work item PR link updated in Azure DevOps"
        );
        Ok((updated, flags))
    }

    /// Mark a work item as a duplicate of another via the provider's native mechanism
    /// (ADO: Duplicate Of link; GitLab: `/duplicate`; GitHub: label + close). Explicit,
    /// user-initiated. Re-stores + re-evaluates the returned item like the other single-
    /// item write-backs.
    pub async fn mark_work_item_duplicate(
        &self,
        team: &str,
        work_item_id: i64,
        duplicate_of: i64,
    ) -> anyhow::Result<(WorkItem, Vec<Flag>)> {
        let provider = self.provider_for(team).await?;
        let mut updated = provider.mark_duplicate(work_item_id, duplicate_of).await?;
        self.store
            .upsert_work_items(&self.owner, std::slice::from_ref(&updated))
            .await?;
        let flags = self.evaluate_scoped(std::slice::from_ref(&updated)).await;
        self.attach_linked_prs(std::slice::from_mut(&mut updated), Some(team))
            .await?;
        info!(work_item_id, duplicate_of, "work item marked as duplicate");
        Ok((updated, flags))
    }

    /// Apply an edit to a work item's state/tags: write it through to the
    /// provider (Azure DevOps), then persist the provider's canonical result
    /// locally so the UI reflects it immediately, before the next poll. Returns
    /// the updated item. A write requires the team's credential to have write
    /// permission on the item.
    /// Returns the updated item **plus its freshly-evaluated hygiene flags**, so
    /// the caller can refresh the Flags column without re-polling everything -
    /// the flags are recomputed here with the team's effective ruleset (the same
    /// engine used on read), reflecting the edit immediately.
    #[tracing::instrument(skip(self, update), fields(team = %team, id))]
    pub async fn update_work_item(
        &self,
        team: &str,
        id: i64,
        update: WorkItemUpdate,
    ) -> anyhow::Result<(WorkItem, Vec<Flag>)> {
        if update.is_empty() {
            anyhow::bail!("no fields to update");
        }
        let provider = self.provider_for(team).await?;
        let updated = provider.update_work_item(id, &update).await?;
        self.store
            .upsert_work_items(&self.owner, std::slice::from_ref(&updated))
            .await?;
        let flags = self.evaluate_scoped(std::slice::from_ref(&updated)).await;
        info!(state = ?update.state, tags = ?update.tags, flags = flags.len(), "work item updated in Azure DevOps");
        Ok((updated, flags))
    }

    /// The provider-discovered set of editable fields for a work item - what the
    /// editor modal renders. Type-specific on Azure DevOps (a Bug's Repro Steps, a
    /// Story's Acceptance Criteria); title + body on GitHub/GitLab. Rich fields come
    /// back as markdown. Read-only; needs the provider's read scope.
    pub async fn work_item_fields(
        &self,
        team: &str,
        id: i64,
    ) -> anyhow::Result<Vec<poseidon_core::EditableField>> {
        let provider = self.provider_for(team).await?;
        Ok(provider.editable_fields(id).await?)
    }

    /// Write changed work-item fields back to the provider, then store + re-evaluate
    /// the returned item. The editor modal's Save - explicit and user-initiated, the
    /// same write-back contract as State/Tags edits. Needs provider write scope.
    pub async fn update_work_item_fields(
        &self,
        team: &str,
        id: i64,
        changes: Vec<poseidon_core::FieldChange>,
    ) -> anyhow::Result<(WorkItem, Vec<Flag>)> {
        if changes.is_empty() {
            anyhow::bail!("no fields to update");
        }
        let provider = self.provider_for(team).await?;
        let updated = provider.update_fields(id, &changes).await?;
        self.store
            .upsert_work_items(&self.owner, std::slice::from_ref(&updated))
            .await?;
        let flags = self.evaluate_scoped(std::slice::from_ref(&updated)).await;
        info!(fields = changes.len(), "work item fields updated");
        Ok((updated, flags))
    }

    /// AI-draft (or improve) the text of ONE field, using the item's context (type,
    /// title, sibling fields) + the team background. Returns markdown for the editor to
    /// drop into the field - never written back automatically; the person reviews and
    /// Saves. Needs an online AI backend (the keyword/embedded backends decline).
    pub async fn draft_work_item_field(
        &self,
        team: &str,
        id: i64,
        field_reference: &str,
        improve: bool,
        working: &[poseidon_core::FieldChange],
    ) -> anyhow::Result<DraftOutcome> {
        let mut fields = self.provider_for(team).await?.editable_fields(id).await?;
        // Overlay the editor's UNSAVED working values so the AI operates on what's ON
        // SCREEN, not the last-saved provider state - so successive drafts compose (a
        // just-generated body feeds a later title improve) and an improve refines the
        // user's current edits. Field metadata (label/kind) stays from the provider.
        if !working.is_empty() {
            let overlay: std::collections::HashMap<&str, &str> = working
                .iter()
                .map(|c| (c.reference.as_str(), c.value.as_str()))
                .collect();
            for f in &mut fields {
                if let Some(v) = overlay.get(f.reference.as_str()) {
                    f.value = (*v).to_string();
                }
            }
        }
        let target = fields
            .iter()
            .find(|f| f.reference == field_reference)
            .ok_or_else(|| anyhow::anyhow!("no editable field '{field_reference}'"))?;
        // Title: prefer the (possibly-edited) Title FIELD, else the stored item title.
        let items = self
            .store
            .list_work_items(&self.owner, Some(team))
            .await
            .unwrap_or_default();
        let item = items.iter().find(|w| w.id == id);
        let title = fields
            .iter()
            .find(|f| f.reference.eq_ignore_ascii_case("title") || f.reference == "System.Title")
            .map(|f| f.value.clone())
            .filter(|t| !t.trim().is_empty())
            .or_else(|| item.map(|w| w.title.clone()))
            .unwrap_or_default();
        let work_item_type = item.map(|w| w.work_item_type.clone()).unwrap_or_default();
        // Sibling context: other non-empty narrative fields (skip the target itself).
        let other_fields: Vec<(String, String)> = fields
            .iter()
            .filter(|f| {
                f.reference != field_reference
                    && f.kind.is_draftable()
                    && !f.value.trim().is_empty()
            })
            .map(|f| (f.label.clone(), f.value.clone()))
            .collect();
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let rules = rules_for_team(&cfg, team);
        let background = rules.team_background.clone().unwrap_or_default();
        let ctx = poseidon_ai::FieldDraftContext {
            work_item_type,
            title,
            field_label: target.label.clone(),
            current_value: target.value.clone(),
            other_fields,
            background,
            mode: if improve {
                poseidon_ai::DraftMode::Improve
            } else {
                poseidon_ai::DraftMode::Draft
            },
            acceptance_criteria_gwt: rules.acceptance_criteria_gwt(),
        };
        // Prefer a SERVER-side model (an online provider). If there's none, or it can't
        // draft (the browser-run WebGPU tagger has no server presence, and the small
        // embedded model declines), hand the built prompt back so the browser can run
        // the SAME model the tagger uses there. So drafting reuses whatever AI tagging
        // uses - no separate "online model" requirement.
        if let Some(ai) = self.ai_tagger().await {
            match ai.draft_field(&ctx).await {
                Ok(value) => return Ok(DraftOutcome::Value(value)),
                Err(poseidon_ai::AiError::Unsupported(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(DraftOutcome::Prompt {
            system: poseidon_ai::FIELD_DRAFT_SYSTEM_PROMPT.to_string(),
            user: poseidon_ai::build_field_draft_prompt(&ctx),
        })
    }

    /// Assemble the whole-item consistency context from the item's editable fields with
    /// the editor's UNSAVED working values overlaid - the draftable (rich-text) fields
    /// only, since those are what "Improve all" proposes. Shared by the run + parse paths
    /// so both agree on which fields (and references) are in scope.
    async fn consistency_context(
        &self,
        team: &str,
        id: i64,
        working: &[poseidon_core::FieldChange],
    ) -> anyhow::Result<poseidon_ai::FieldsConsistencyContext> {
        let mut fields = self.provider_for(team).await?.editable_fields(id).await?;
        if !working.is_empty() {
            let overlay: std::collections::HashMap<&str, &str> = working
                .iter()
                .map(|c| (c.reference.as_str(), c.value.as_str()))
                .collect();
            for f in &mut fields {
                if let Some(v) = overlay.get(f.reference.as_str()) {
                    f.value = (*v).to_string();
                }
            }
        }
        let items = self
            .store
            .list_work_items(&self.owner, Some(team))
            .await
            .unwrap_or_default();
        let item = items.iter().find(|w| w.id == id);
        let title = fields
            .iter()
            .find(|f| f.reference.eq_ignore_ascii_case("title") || f.reference == "System.Title")
            .map(|f| f.value.clone())
            .filter(|t| !t.trim().is_empty())
            .or_else(|| item.map(|w| w.title.clone()))
            .unwrap_or_default();
        let work_item_type = item.map(|w| w.work_item_type.clone()).unwrap_or_default();
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let team_rules = rules_for_team(&cfg, team);
        let background = team_rules.team_background.clone().unwrap_or_default();
        let acceptance_criteria_gwt = team_rules.acceptance_criteria_gwt();
        // Only the rich narrative fields (Description, Repro Steps, Acceptance Criteria,
        // …) - a consistency pass over a treePath (Area Path), a pick-list, or the Title
        // is meaningless, and those `Text` kinds are draftable too, so filter by the rich
        // kinds explicitly rather than `is_draftable()`. The title is context (above).
        let harmonised: Vec<poseidon_ai::DraftFieldValue> = fields
            .iter()
            .filter(|f| {
                !f.read_only
                    && matches!(
                        f.kind,
                        poseidon_core::FieldKind::Markdown | poseidon_core::FieldKind::PlainText
                    )
            })
            .map(|f| poseidon_ai::DraftFieldValue {
                reference: f.reference.clone(),
                label: f.label.clone(),
                value: f.value.clone(),
            })
            .collect();
        Ok(poseidon_ai::FieldsConsistencyContext {
            work_item_type,
            title,
            background,
            fields: harmonised,
            acceptance_criteria_gwt,
        })
    }

    /// Refine ALL of an item's proposed rich fields into a mutually-consistent set in one
    /// pass. Server-run when an online model is configured; otherwise the built prompt is
    /// handed back for the browser to run (WebGPU), which posts the reply to
    /// [`Self::parse_refine_reply`]. `working` carries the editor's unsaved values.
    pub async fn refine_work_item_fields(
        &self,
        team: &str,
        id: i64,
        working: &[poseidon_core::FieldChange],
    ) -> anyhow::Result<RefineOutcome> {
        let ctx = self.consistency_context(team, id, working).await?;
        if ctx.fields.is_empty() {
            return Ok(RefineOutcome::Value(Vec::new())); // nothing to harmonise
        }
        if let Some(ai) = self.ai_tagger().await {
            match ai.refine_fields(&ctx).await {
                Ok(pairs) => {
                    return Ok(RefineOutcome::Value(
                        pairs
                            .into_iter()
                            .map(|(reference, value)| poseidon_core::FieldChange {
                                reference,
                                value,
                            })
                            .collect(),
                    ))
                }
                Err(poseidon_ai::AiError::Unsupported(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(RefineOutcome::Prompt {
            system: poseidon_ai::FIELDS_CONSISTENCY_SYSTEM_PROMPT.to_string(),
            user: poseidon_ai::build_fields_consistency_prompt(&ctx),
        })
    }

    /// Validate a browser (WebGPU) consistency reply and turn it into field changes. The
    /// trust boundary for that path: the reply is re-parsed server-side and kept only for
    /// references that are actually editable on this item. `working` scopes which fields
    /// were in the request (same set as the run path).
    pub async fn parse_refine_reply(
        &self,
        team: &str,
        id: i64,
        working: &[poseidon_core::FieldChange],
        text: &str,
    ) -> anyhow::Result<Vec<poseidon_core::FieldChange>> {
        let ctx = self.consistency_context(team, id, working).await?;
        let refs: Vec<String> = ctx.fields.iter().map(|f| f.reference.clone()).collect();
        Ok(poseidon_ai::parse_fields_consistency(text, &refs)
            .into_iter()
            .map(|(reference, value)| poseidon_core::FieldChange { reference, value })
            .collect())
    }

    /// Hygiene flags for the current stored items (optionally one team),
    /// evaluated against each item's team-effective ruleset as of now.
    pub async fn flags(&self, team: Option<&str>) -> anyhow::Result<Vec<Flag>> {
        let items = self.store.list_work_items(&self.owner, team).await?;
        Ok(self.evaluate_scoped(&items).await)
    }

    /// Evaluate hygiene flags with each item judged by ITS team's effective
    /// ruleset. A read may span teams (the "All teams" roll-up), and each team
    /// can override the instance-wide default - so we partition by team and run
    /// the engine per group against `rules_for(team)`. An item whose team is no
    /// longer in config falls back to the global default. Grouped via `BTreeMap`
    /// so the flag order is deterministic (by team name) across calls.
    async fn evaluate_scoped(&self, items: &[WorkItem]) -> Vec<Flag> {
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let now = Utc::now();
        let mut groups: std::collections::BTreeMap<&str, Vec<WorkItem>> =
            std::collections::BTreeMap::new();
        for it in items {
            groups.entry(it.team.as_str()).or_default().push(it.clone());
        }
        let mut flags = Vec::new();
        for (team_name, group) in groups {
            let rules = cfg
                .teams
                .iter()
                .find(|t| t.name == team_name)
                .map(|t| cfg.rules_for(t))
                .unwrap_or(&cfg.rules);
            flags.extend(poseidon_rules::evaluate(&group, rules, now));
        }
        // Merge stored on-demand AI healthcheck findings as `ai_audit` flags, so they
        // ride the same chips / dashboard counts / `?flag=` filter as the deterministic
        // ones. Advisory (Warn) and only present for items a person audited; scoped to
        // the items in this read via their id + team.
        let team_of: HashMap<i64, String> = items.iter().map(|i| (i.id, i.team.clone())).collect();
        // Stored near-duplicate findings (from the on-demand scan) as flags, scoped to
        // the items in this read.
        if let Ok(dups) = self.store.near_duplicates(&self.owner, None).await {
            for (id, detail) in dups {
                if let Some(team) = team_of.get(&id) {
                    flags.push(Flag {
                        work_item_id: id,
                        team: team.clone(),
                        code: FlagCode::NearDuplicate,
                        severity: Severity::Warn,
                        message: format!("possible duplicate: {detail}"),
                        tag: None,
                    });
                }
            }
        }
        if let Ok(audit) = self.store.ai_audit(&self.owner, None).await {
            for (id, findings) in audit {
                let Some(team) = team_of.get(&id) else {
                    continue; // finding for an item not in this read's scope
                };
                for (kind, detail) in findings {
                    flags.push(Flag {
                        work_item_id: id,
                        team: team.clone(),
                        code: FlagCode::AiAudit,
                        severity: Severity::Warn,
                        message: format!(
                            "AI healthcheck ({}): {}",
                            audit_kind_label(&kind),
                            detail
                        ),
                        tag: None,
                    });
                }
            }
        }
        flags
    }

    /// Live pipeline health (optionally one team), folded from stored pipelines
    /// + recent runs.
    pub async fn pipeline_health(&self, team: Option<&str>) -> anyhow::Result<Vec<PipelineHealth>> {
        let since = Utc::now() - chrono::Duration::days(HEALTH_WINDOW_DAYS);
        let pipelines = self.store.list_pipelines(&self.owner, team).await?;
        let runs = self.store.list_runs(&self.owner, since, team).await?;
        let mut health = fold_pipeline_health(&pipelines, &runs);
        // Attach pipeline hygiene flags using each pipeline's team-effective rules.
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        for h in &mut health {
            let rules = rules_for_team(&cfg, &h.team);
            h.flags = poseidon_rules::evaluate_pipeline(h.last_status, &rules.pipelines);
        }
        Ok(health)
    }

    /// Stored pull requests, optionally scoped to one team, with hygiene flags
    /// (stale-open / stale-draft) attached per the team's effective rules.
    pub async fn pull_requests(&self, team: Option<&str>) -> anyhow::Result<Vec<PullRequest>> {
        let mut prs = self.store.list_pull_requests(&self.owner, team).await?;
        // The PR screen shows the in-flight (active) set only. Completed and
        // abandoned PRs are polled + stored too, but purely to colour work-item
        // link chips - they'd be noise here, so drop them from this list.
        prs.retain(|p| p.status == PrStatus::Active);
        // Reverse the work-item -> PR links to get PR -> work items.
        let items = self.store.list_work_items(&self.owner, team).await?;
        let mut by_pr: HashMap<i64, Vec<i64>> = HashMap::new();
        for it in &items {
            for pr_id in &it.linked_pr_ids {
                by_pr.entry(*pr_id).or_default().push(it.id);
            }
        }
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let now = Utc::now();
        for pr in &mut prs {
            pr.linked_work_items = by_pr.get(&pr.id).cloned().unwrap_or_default();
            let rules = rules_for_team(&cfg, &pr.team);
            pr.flags = poseidon_rules::evaluate_pull_request(pr, &rules.pull_requests, now);
        }
        Ok(prs)
    }

    /// Everything the dashboard needs in one call, optionally scoped to a team.
    pub async fn dashboard(&self, team: Option<&str>) -> anyhow::Result<DashboardSummary> {
        let items = self.store.list_work_items(&self.owner, team).await?;
        let flags = self.evaluate_scoped(&items).await;
        let pipelines = self.pipeline_health(team).await?;
        // Active PRs with hygiene flags + links already attached (same view as the
        // PR screen), so PR rollups match what the PR list shows.
        let prs = self.pull_requests(team).await?;
        let last_polled_at = self.store.get_meta(&self.owner, "last_polled_at").await?;

        let flagged_items = flags
            .iter()
            .map(|f| f.work_item_id)
            .collect::<HashSet<_>>()
            .len() as i64;

        // Open = active & not draft; Draft = active & draft. `prs` is active-only.
        let (open_prs, draft_prs) = prs.iter().fold((0i64, 0i64), |(open, draft), p| {
            if p.is_draft {
                (open, draft + 1)
            } else {
                (open + 1, draft)
            }
        });

        // Rollups over the entity flags now carried by PRs + pipelines.
        let flagged_prs = prs.iter().filter(|p| !p.flags.is_empty()).count() as i64;
        let flagged_pipelines = pipelines.iter().filter(|p| !p.flags.is_empty()).count() as i64;
        let pr_flags_by_code = count_entity_flags(prs.iter().flat_map(|p| &p.flags));
        let pipeline_flags_by_code = count_entity_flags(pipelines.iter().flat_map(|p| &p.flags));

        Ok(DashboardSummary {
            total_work_items: items.len() as i64,
            flagged_items,
            flags_by_code: count_by_code(&flags),
            pipelines,
            open_prs,
            draft_prs,
            flagged_prs,
            pr_flags_by_code,
            flagged_pipelines,
            pipeline_flags_by_code,
            last_polled_at,
        })
    }

    /// Ticket-flow report over a date range (RFC3339 bounds), optionally scoped.
    pub async fn ticket_report(
        &self,
        from: &str,
        to: &str,
        team: Option<&str>,
    ) -> anyhow::Result<TicketReport> {
        Ok(self
            .store
            .ticket_report(&self.owner, from, to, team)
            .await?)
    }

    /// Pipeline-flow report over a date range (RFC3339 bounds), optionally scoped.
    pub async fn pipeline_report(
        &self,
        from: &str,
        to: &str,
        team: Option<&str>,
    ) -> anyhow::Result<PipelineReport> {
        Ok(self
            .store
            .pipeline_report(&self.owner, from, to, team)
            .await?)
    }

    // ─────────────────────── Configurable reports ───────────────────────

    /// Every runnable report: the code-defined built-in templates first, then
    /// the owner's saved reports (name-ordered).
    pub async fn report_specs(&self) -> anyhow::Result<Vec<poseidon_core::ReportSpec>> {
        let mut specs = poseidon_reports::builtins();
        specs.extend(self.store.list_reports(&self.owner).await?);
        Ok(specs)
    }

    /// Find a spec by name across built-ins + saved.
    async fn find_spec(&self, name: &str) -> anyhow::Result<poseidon_core::ReportSpec> {
        if let Some(b) = poseidon_reports::builtins()
            .into_iter()
            .find(|s| s.name == name)
        {
            return Ok(b);
        }
        self.store
            .get_report(&self.owner, name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown report \"{name}\""))
    }

    /// Run a saved/built-in report by name, optionally overriding its team scope
    /// with the caller's current scope (the UI team selector).
    pub async fn run_report_named(
        &self,
        name: &str,
        team_override: Option<&str>,
    ) -> anyhow::Result<poseidon_core::ReportResult> {
        let spec = self.find_spec(name).await?;
        self.run_report_spec(spec, team_override).await
    }

    /// Run an arbitrary (unsaved) spec - powers the Reports builder's live
    /// preview. `team_override`, when set, replaces the spec's own team scope.
    pub async fn run_report_spec(
        &self,
        mut spec: poseidon_core::ReportSpec,
        team_override: Option<&str>,
    ) -> anyhow::Result<poseidon_core::ReportResult> {
        if let Some(t) = team_override {
            spec.team = Some(t.to_string());
        }
        let data = self.load_datasets(&spec).await?;
        Ok(poseidon_reports::run(&spec, &data, Utc::now()))
    }

    /// Load only the datasets a spec references, scoped to its team.
    async fn load_datasets(
        &self,
        spec: &poseidon_core::ReportSpec,
    ) -> anyhow::Result<poseidon_reports::Datasets> {
        use poseidon_core::DataSource;
        let team = spec.team.as_deref();
        let uses = |src: DataSource| spec.series.iter().any(|s| s.source == src);
        let mut data = poseidon_reports::Datasets::default();
        if uses(DataSource::WorkItems) {
            data.work_items = self.store.list_work_items(&self.owner, team).await?;
        }
        if uses(DataSource::PullRequests) {
            // All stored PRs (active + completed + abandoned) - reports may want
            // the closed ones (e.g. merge rate), unlike the active-only PR screen.
            data.pull_requests = self.store.list_pull_requests(&self.owner, team).await?;
        }
        if uses(DataSource::Pipelines) {
            data.pipelines = self.pipeline_health(team).await?;
        }
        if uses(DataSource::PipelineRuns) {
            let since = report_runs_since(&spec.time_range);
            data.runs = self.store.list_runs(&self.owner, since, team).await?;
        }
        Ok(data)
    }

    /// Save a user report. Rejects overwriting a built-in template's name - the
    /// UI turns that into a "save as" prompt for a fresh name.
    pub async fn save_report(&self, mut spec: poseidon_core::ReportSpec) -> anyhow::Result<()> {
        if poseidon_reports::builtins()
            .iter()
            .any(|b| b.name == spec.name)
        {
            anyhow::bail!(
                "\"{}\" is a built-in report; save it under a new name",
                spec.name
            );
        }
        if spec.name.trim().is_empty() {
            anyhow::bail!("report name is required");
        }
        spec.builtin = false; // a saved report is never a template
        self.store.upsert_report(&self.owner, &spec).await?;
        Ok(())
    }

    /// Delete a saved report. Built-ins can't be deleted.
    pub async fn delete_report(&self, name: &str) -> anyhow::Result<bool> {
        if poseidon_reports::builtins().iter().any(|b| b.name == name) {
            anyhow::bail!("\"{name}\" is a built-in report and can't be deleted");
        }
        Ok(self.store.delete_report(&self.owner, name).await?)
    }
}

/// How far back to load pipeline runs for a report, from its time range. A
/// generous default covers `AllTime` / `Between` (the engine still applies the
/// exact window); relative ranges load just their span plus a small buffer.
fn report_runs_since(range: &poseidon_core::TimeRange) -> chrono::DateTime<Utc> {
    let days = match range {
        poseidon_core::TimeRange::LastDays { days } => days + 2,
        _ => 3650,
    };
    Utc::now() - chrono::Duration::days(days)
}

/// Human-facing wrapper so `Arc<Service>` reads naturally at call sites.
pub type SharedService = Arc<Service>;

/// Stable string for a flag code - the key the dashboard renders. Matches the
/// serde `snake_case` form so the UI can pair it with a label table.
fn code_str(code: FlagCode) -> &'static str {
    match code {
        FlagCode::Untagged => "untagged",
        FlagCode::MissingRequiredTag => "missing_required_tag",
        FlagCode::DisallowedTag => "disallowed_tag",
        FlagCode::Stale => "stale",
        FlagCode::StaleStateTag => "stale_state_tag",
        FlagCode::Underspecified => "underspecified",
        FlagCode::Duplicate => "duplicate",
        FlagCode::BadTitle => "bad_title",
        FlagCode::NearDuplicate => "near_duplicate",
        FlagCode::OrphanedChild => "orphaned_child",
        FlagCode::AiAudit => "ai_audit",
    }
}

/// Human label for an audit finding's kind slug (see [`poseidon_ai::AuditKind`]),
/// shown as the prefix of an `ai_audit` flag's message. Falls back to the raw slug
/// for any future kind not yet mapped here.
fn audit_kind_label(kind: &str) -> &str {
    match kind {
        "unclear" => "unclear",
        "bad_title" => "vague title",
        "bad_data" => "bad data",
        other => other,
    }
}

/// Count flags by code, descending - the dashboard's "flags by kind" row.
fn count_by_code(flags: &[Flag]) -> Vec<TagCount> {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for f in flags {
        *counts.entry(code_str(f.code)).or_default() += 1;
    }
    let mut out: Vec<TagCount> = counts
        .into_iter()
        .map(|(tag, count)| TagCount {
            tag: tag.to_string(),
            count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    out
}

/// Count entity flags (pipelines / PRs) by their string code, descending. The
/// EntityFlag `code` is already the display slug, so no mapping is needed.
fn count_entity_flags<'a>(flags: impl IntoIterator<Item = &'a EntityFlag>) -> Vec<TagCount> {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for f in flags {
        *counts.entry(f.code.as_str()).or_default() += 1;
    }
    let mut out: Vec<TagCount> = counts
        .into_iter()
        .map(|(tag, count)| TagCount {
            tag: tag.to_string(),
            count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    out
}

/// The effective ruleset for a team by name: its `[team.rules]` override if any,
/// else the instance default. Used to attach per-team hygiene flags on read.
fn rules_for_team<'a>(
    cfg: &'a poseidon_core::UserConfig,
    team: &str,
) -> &'a poseidon_core::RuleSet {
    cfg.teams
        .iter()
        .find(|t| t.name == team)
        .map(|t| cfg.rules_for(t))
        .unwrap_or(&cfg.rules)
}

/// Convert the config's column mapping into the provider crate's `FieldMap` (core
/// can't depend on providers, so the config carries a parallel shape). An empty
/// column name falls back to the Port "Service" export default for that field.
fn field_map_from_config(m: &poseidon_core::CatalogFieldMap) -> FieldMap {
    let def = FieldMap::port_service_export();
    let or = |s: &str, d: String| {
        if s.trim().is_empty() {
            d
        } else {
            s.to_string()
        }
    };
    FieldMap {
        product: or(&m.product, def.product),
        team: or(&m.team, def.team),
        repo_source: or(&m.repo_source, def.repo_source),
        kind: m.kind.clone().or(def.kind),
        domain: m.domain.clone().or(def.domain),
    }
}

/// A cached device-code token set, persisted per owner as JSON on the data
/// volume (the `az` token cache's native replacement; never the DB).
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: chrono::DateTime<Utc>,
}

impl From<poseidon_providers::oauth::TokenSet> for CachedToken {
    fn from(t: poseidon_providers::oauth::TokenSet) -> Self {
        Self {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: t.expires_at,
        }
    }
}

fn read_cached_token(path: &std::path::Path) -> Option<CachedToken> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn write_cached_token(path: &std::path::Path, token: &CachedToken) {
    if let Ok(json) = serde_json::to_vec_pretty(token) {
        let _ = std::fs::write(path, json);
    }
}

/// Read a cached device-code token, silently refreshing it when near expiry.
/// `Err` (surfaced as "not signed in") when there's no usable session. Shared by
/// the service credential path and the doctor Azure-access check.
pub(crate) async fn acquire_cached_token(
    path: &std::path::Path,
    tenant: Option<&str>,
) -> Result<String, String> {
    let Some(cached) = read_cached_token(path) else {
        return Err("no Azure session - sign in".to_string());
    };
    if cached.expires_at > Utc::now() + chrono::Duration::seconds(60) {
        return Ok(cached.access_token);
    }
    let Some(rt) = cached.refresh_token.as_deref() else {
        return Err("Azure session expired - sign in again".to_string());
    };
    match poseidon_providers::oauth::refresh(&reqwest::Client::new(), tenant, rt).await {
        Ok(ts) => {
            let fresh = CachedToken::from(ts);
            write_cached_token(path, &fresh);
            Ok(fresh.access_token)
        }
        Err(e) => Err(format!("Azure session expired - sign in again ({e})")),
    }
}

/// The concrete candidate tags handed to the AI for one item's team: real,
/// already-approved values it may pick from, built from the team's required +
/// allowed patterns resolved to concrete tags. A literal pattern (`type:bug`) is
/// itself a candidate. A wildcard pattern (`area:*`) is an open-vocabulary slot
/// with no literal to offer, so it's expanded into the concrete values already in
/// use for that prefix - drawn from the team's own configured taxonomy (each
/// keyword rule's canonical `tag`, each alias's canonical `to`) and every tag
/// observed across the team's backlog (`observed`).
///
/// Everything is filtered back through the approved patterns (so a stray backlog
/// tag outside the taxonomy is never offered), de-duplicated case-insensitively,
/// keeping each value's first-seen spelling. This is what lets the AI satisfy a
/// wildcard-required slot (`area:*`/`source:*`) by choosing a real curated value -
/// never coining a fresh one, which would fragment the taxonomy the aliases exist
/// to keep tidy. Empty means the team has nothing concrete to offer (no literals,
/// and no observed/configured values behind its wildcards), so AI tagging is
/// skipped for it.
/// Per-tag keyword hints for the AI prompt, from the team's keyword rules: the
/// canonical tag (lowercased) -> its configured keywords. Lets the model see what
/// each candidate actually means (e.g. `area:platform-deployment` keys on "platform
/// deployment", not any "platform" mention). See [`poseidon_ai::TagHints`].
fn tag_hints(rules: &poseidon_core::RuleSet) -> poseidon_ai::TagHints {
    rules
        .tag_keywords
        .iter()
        .filter(|k| !k.tag.trim().is_empty() && !k.keywords.is_empty())
        .map(|k| (k.tag.trim().to_lowercase(), k.keywords.clone()))
        .collect()
}

fn candidate_tags(rules: &poseidon_core::RuleSet, observed: &[String]) -> Vec<String> {
    let patterns: Vec<&str> = rules
        .required_tags
        .iter()
        .chain(rules.allowed_tags.iter())
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if patterns.is_empty() {
        return Vec::new();
    }
    // Pool of concrete (non-wildcard) values to draw from: the patterns
    // themselves (literals survive the filter below; wildcards don't), the
    // canonical tag of every keyword + alias rule, and every observed backlog
    // tag. Filtered to only those the approved patterns actually admit.
    let mut seen = std::collections::HashSet::new();
    patterns
        .iter()
        .map(|t| t.to_string())
        .chain(rules.tag_keywords.iter().map(|k| k.tag.trim().to_string()))
        .chain(rules.tag_aliases.iter().map(|a| a.to.trim().to_string()))
        .chain(observed.iter().map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty() && !t.contains('*'))
        .filter(|t| patterns.iter().any(|p| poseidon_rules::tag_matches(p, t)))
        .filter(|t| seen.insert(t.to_lowercase()))
        .collect()
}

#[cfg(test)]
mod ai_tests {
    use super::candidate_tags;
    use poseidon_core::{RuleSet, TagAlias, TagKeywords};

    #[test]
    fn candidate_tags_drops_bare_wildcards_and_dedups() {
        // With no observed/configured values behind them, wildcard patterns
        // contribute nothing; only the concrete literals survive.
        let rules = RuleSet {
            required_tags: vec!["type:*".into(), "team:platform".into()],
            allowed_tags: vec![
                "type:bug".into(),
                "TYPE:BUG".into(),
                "priority:*".into(),
                " ".into(),
            ],
            ..Default::default()
        };
        let got = candidate_tags(&rules, &[]);
        assert_eq!(
            got,
            vec!["team:platform".to_string(), "type:bug".to_string()]
        );
    }

    #[test]
    fn candidate_tags_empty_when_only_wildcards_and_nothing_observed() {
        // A team whose whole approved set is patterns, with no values in use behind
        // them, has nothing concrete to offer - AI suggestion is skipped for it.
        let rules = RuleSet {
            required_tags: vec!["type:*".into()],
            allowed_tags: vec!["team:*".into(), "  ".into()],
            ..Default::default()
        };
        assert!(candidate_tags(&rules, &[]).is_empty());
        // Nothing configured at all -> also empty.
        assert!(candidate_tags(&RuleSet::default(), &[]).is_empty());
    }

    #[test]
    fn candidate_tags_keeps_configured_spelling_and_order() {
        // Required tags lead, then allowed; each kept in its first-seen spelling.
        let rules = RuleSet {
            required_tags: vec!["Area:Data".into()],
            allowed_tags: vec!["area:data".into(), "Type:Bug".into()],
            ..Default::default()
        };
        // "area:data" from allowed is a case-dupe of the required "Area:Data" and is
        // dropped, keeping the required spelling; "Type:Bug" survives.
        assert_eq!(
            candidate_tags(&rules, &[]),
            vec!["Area:Data".to_string(), "Type:Bug".to_string()]
        );
    }

    #[test]
    fn candidate_tags_expands_wildcards_from_observed_and_config() {
        // The real-world case: open-vocabulary required slots (`area:*`/`source:*`)
        // are filled from concrete values in the taxonomy (a keyword's canonical
        // tag, an alias's canonical target) and values already on the backlog.
        let rules = RuleSet {
            required_tags: vec!["area:*".into(), "source:*".into()],
            allowed_tags: vec!["enhancement".into()],
            tag_keywords: vec![TagKeywords {
                tag: "source:internal".into(),
                keywords: vec!["internal".into()],
            }],
            tag_aliases: vec![TagAlias {
                from: "fe".into(),
                to: "area:frontend".into(),
            }],
            ..Default::default()
        };
        // Observed on the backlog: a real area value, a case-dupe of it, and a tag
        // outside the taxonomy that must NOT be offered.
        let observed = vec![
            "area:mobile".to_string(),
            "AREA:Mobile".to_string(),
            "random".to_string(),
        ];
        let got = candidate_tags(&rules, &observed);

        assert!(
            got.contains(&"enhancement".to_string()),
            "literal allowed tag"
        );
        assert!(got.contains(&"area:frontend".to_string()), "from alias .to");
        assert!(
            got.contains(&"source:internal".to_string()),
            "from keyword .tag"
        );
        assert!(got.contains(&"area:mobile".to_string()), "from the backlog");
        assert!(
            !got.iter().any(|t| t.eq_ignore_ascii_case("random")),
            "a tag outside the approved patterns is never offered"
        );
        assert_eq!(
            got.iter()
                .filter(|t| t.eq_ignore_ascii_case("area:mobile"))
                .count(),
            1,
            "case-duplicates collapse"
        );
    }
}

/// Outcome of an AI tag-suggestion run, over the scoped items.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AiSuggestSummary {
    /// Items sent to the model (those whose team has an approved tag set).
    pub considered: usize,
    /// Items that came back with at least one suggestion.
    pub with_suggestions: usize,
    /// Total suggestions stored across all items.
    pub suggestions: usize,
}

/// Outcome of a near-duplicate scan over the scoped items.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DupScanSummary {
    /// Items considered.
    pub scanned: usize,
    /// Items that resemble at least one other (i.e. carry a near_duplicate flag).
    pub flagged: usize,
    /// Total match edges recorded across all flagged items.
    pub pairs: usize,
}

/// Outcome of an AI healthcheck audit run, over the scoped items.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AiAuditSummary {
    /// Items sent to the model to judge.
    pub considered: usize,
    /// Items that came back with at least one concern.
    pub flagged: usize,
    /// Total concerns stored across all items.
    pub findings: usize,
}

/// One item's prompt for the browser (WebGPU) audit path: the exact system + user
/// messages the server built, so the browser runs the SAME prompt on its local
/// model and posts the raw reply back to [`Service::store_healthcheck_audit`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditPrompt {
    pub id: i64,
    pub system: String,
    pub user: String,
}

/// A browser-computed (WebGPU) audit reply for one item: the raw model text, which
/// is parsed and stored server-side (the trust boundary - the browser can't inject
/// arbitrary findings; every one is re-validated by `poseidon_ai::parse_audit_response`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrowserAuditResult {
    pub id: i64,
    #[serde(default)]
    pub text: String,
}

/// A browser-computed (WebGPU) suggestion set for one work item, posted back to be
/// re-validated + stored (see [`Service::store_tag_suggestions`]).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrowserSuggestion {
    pub id: i64,
    #[serde(default)]
    pub tags: Vec<BrowserTag>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrowserTag {
    pub tag: String,
    #[serde(default)]
    pub reason: String,
}

/// Fold pipeline definitions + their runs into per-pipeline health. Pure so
/// it's unit-testable without a store.
fn fold_pipeline_health(
    pipelines: &[poseidon_core::Pipeline],
    runs: &[poseidon_core::PipelineRun],
) -> Vec<PipelineHealth> {
    // Runs arrive newest-first (the store orders by started_at DESC); rely on
    // that for "last run" / "last status".
    let mut by_pipeline: HashMap<i64, Vec<&poseidon_core::PipelineRun>> = HashMap::new();
    for r in runs {
        by_pipeline.entry(r.pipeline_id).or_default().push(r);
    }

    pipelines
        .iter()
        .map(|p| {
            let prs = by_pipeline.get(&p.id).cloned().unwrap_or_default();
            let (mut succeeded, mut failed, mut running) = (0i64, 0i64, 0i64);
            for r in &prs {
                match r.status {
                    RunStatus::Succeeded => succeeded += 1,
                    RunStatus::Failed => failed += 1,
                    RunStatus::Running => running += 1,
                    _ => {}
                }
            }
            // Newest run within the window (may be None for a pipeline that last
            // ran before it). Fall back to the pipeline's real last completed run
            // (from includeLatestBuilds) so an old-but-real pipeline shows its
            // actual status instead of "never run".
            let last = prs.first();
            let last_status = last.map(|r| r.status).or(p.last_run_status);
            let last_run_url = last
                .map(|r| r.url.clone())
                .or_else(|| p.last_run_url.clone());
            let last_run_at = last
                .and_then(|r| r.finished_at.or(r.started_at))
                .or(p.last_run_at)
                .map(|d| d.to_rfc3339());
            let last_failure_at = prs
                .iter()
                .find(|r| r.status == RunStatus::Failed)
                .and_then(|r| r.started_at)
                .map(|d| d.to_rfc3339())
                .or_else(|| {
                    // No failure in the window, but the real last run failed.
                    (p.last_run_status == Some(RunStatus::Failed))
                        .then(|| p.last_run_at.map(|d| d.to_rfc3339()))
                        .flatten()
                });

            PipelineHealth {
                pipeline_id: p.id,
                name: p.name.clone(),
                url: p.url.clone(),
                folder: p.folder.clone(),
                team: p.team.clone(),
                last_status,
                last_failure_at,
                last_run_at,
                last_run_url,
                succeeded,
                failed,
                running,
                flags: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use poseidon_core::{Pipeline, PipelineRun};

    fn run(pipeline_id: i64, status: RunStatus, started: &str) -> PipelineRun {
        PipelineRun {
            id: started.len() as i64 + pipeline_id, // any unique-ish id
            pipeline_id,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            status,
            started_at: DateTime::parse_from_rfc3339(started)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            finished_at: None,
            source_branch: None,
            url: format!("https://example/{started}"),
        }
    }

    #[test]
    fn sanitize_owner_keeps_unreserved_and_collapses_rest() {
        // Emails stay readable; path separators + other chars become `_` so the
        // per-owner az dir can never escape the sessions root.
        assert_eq!(sanitize_owner("a.user@example.com"), "a.user_example.com");
        assert_eq!(
            sanitize_owner("first.last-team@corp.co.uk"),
            "first.last-team_corp.co.uk"
        );
        assert_eq!(sanitize_owner("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_owner("plain"), "plain");
        // A dots-only owner would resolve to the root / its parent - collapse it.
        assert_eq!(sanitize_owner(".."), "_");
        assert_eq!(sanitize_owner("."), "_");
    }

    #[test]
    fn config_bundle_yaml_round_trips() {
        use poseidon_core::{BundleMeta, ConfigBundle, UserConfig, CONFIG_BUNDLE_SCHEMA};
        let bundle = ConfigBundle {
            poseidon: BundleMeta {
                schema: CONFIG_BUNDLE_SCHEMA,
                app_version: "test".into(),
                exported_at: "2026-07-31T00:00:00Z".into(),
                owner: Some("someone@example.com".into()),
            },
            config: UserConfig {
                poll_all_teams: true,
                ..Default::default()
            },
            reports: vec![],
        };
        let yaml = serde_norway::to_string(&bundle).unwrap();
        // Envelope under `poseidon:`; the owner config is flattened to the top
        // level (so `poll_all_teams`, not `config.poll_all_teams`).
        assert!(yaml.contains("schema:"), "yaml:\n{yaml}");
        assert!(yaml.contains("poll_all_teams: true"), "yaml:\n{yaml}");
        let back: ConfigBundle = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(back.poseidon.schema, CONFIG_BUNDLE_SCHEMA);
        assert_eq!(back.poseidon.app_version, "test");
        assert_eq!(back.poseidon.owner.as_deref(), Some("someone@example.com"));
        assert!(back.config.poll_all_teams);

        // Owner is optional: a bundle without it deserializes to None.
        let minimal: ConfigBundle = serde_norway::from_str("poseidon:\n  schema: 1\n").unwrap();
        assert_eq!(minimal.poseidon.owner, None);
        assert!(minimal.config.teams.is_empty());
    }

    // The security contract: the public/HTTP import (`import_config`) always writes
    // the caller's OWN tenant and ignores a bundle's `owner`, so one user can never
    // overwrite another's config by crafting a file. Only the trusted
    // (`import_config_trusted`) path - CLI / startup / backend - honors `owner`.
    #[tokio::test]
    async fn bundle_owner_ignored_by_public_import_but_honored_by_trusted() {
        use poseidon_core::DEFAULT_OWNER;
        let store = Store::connect_in_memory().await.unwrap();
        let svc = Service::new(PoseidonConfig::default(), store, std::env::temp_dir());

        // A bundle that CLAIMS to target victim@example.com.
        let bundle = "poseidon:\n  schema: 1\n  owner: victim@example.com\nteam:\n  - name: Injected\n    provider: stub\n    organization: https://stub.example\n    project: Injected\n";

        // Public path as attacker@example.com: lands under attacker, NOT victim.
        svc.with_owner("attacker@example.com")
            .import_config(bundle, true)
            .await
            .unwrap();
        let attacker = svc
            .with_owner("attacker@example.com")
            .export_config()
            .await
            .unwrap();
        let victim = svc
            .with_owner("victim@example.com")
            .export_config()
            .await
            .unwrap();
        assert!(
            attacker.contains("Injected"),
            "attacker should get the team"
        );
        assert!(
            !victim.contains("Injected"),
            "victim must be untouched - no cross-tenant write"
        );

        // Trusted path honors the bundle's owner: it lands under victim.
        svc.import_config_trusted(bundle, true, None).await.unwrap();
        let victim = svc
            .with_owner("victim@example.com")
            .export_config()
            .await
            .unwrap();
        assert!(victim.contains("Injected"), "trusted import honors owner");

        // Explicit override beats the bundle's owner; default never received it.
        svc.import_config_trusted(bundle, true, Some("ops@example.com"))
            .await
            .unwrap();
        assert!(svc
            .with_owner("ops@example.com")
            .export_config()
            .await
            .unwrap()
            .contains("Injected"));
        assert!(!svc
            .with_owner(DEFAULT_OWNER)
            .export_config()
            .await
            .unwrap()
            .contains("Injected"));
    }

    #[test]
    fn fold_health_picks_latest_status_and_last_failure() {
        let pipelines = vec![Pipeline {
            id: 10,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            name: "platform-ci".into(),
            folder: None,
            url: "https://example/pipeline/10".into(),
            last_run_status: None,
            last_run_at: None,
            last_run_url: None,
        }];
        // Newest-first, as the store returns them.
        let runs = vec![
            run(10, RunStatus::Succeeded, "2026-07-30T10:00:00Z"),
            run(10, RunStatus::Failed, "2026-07-29T10:00:00Z"),
            run(10, RunStatus::Succeeded, "2026-07-28T10:00:00Z"),
        ];
        let health = fold_pipeline_health(&pipelines, &runs);
        assert_eq!(health.len(), 1);
        let h = &health[0];
        assert_eq!(h.last_status, Some(RunStatus::Succeeded));
        assert_eq!(h.succeeded, 2);
        assert_eq!(h.failed, 1);
        assert_eq!(
            h.last_failure_at.as_deref(),
            Some("2026-07-29T10:00:00+00:00")
        );
    }

    #[test]
    fn fold_health_handles_pipeline_with_no_runs() {
        let pipelines = vec![Pipeline {
            id: 99,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            name: "never-run".into(),
            folder: None,
            url: "https://example/pipeline/99".into(),
            last_run_status: None,
            last_run_at: None,
            last_run_url: None,
        }];
        let health = fold_pipeline_health(&pipelines, &[]);
        assert_eq!(health[0].last_status, None);
        assert_eq!(health[0].succeeded, 0);
        assert!(health[0].last_failure_at.is_none());
    }

    #[test]
    fn count_by_code_is_descending() {
        let flags = vec![
            Flag {
                work_item_id: 1,
                team: "P".into(),
                code: FlagCode::Untagged,
                severity: poseidon_core::Severity::Warn,
                message: String::new(),
                tag: None,
            },
            Flag {
                work_item_id: 2,
                team: "P".into(),
                code: FlagCode::Stale,
                severity: poseidon_core::Severity::Warn,
                message: String::new(),
                tag: None,
            },
            Flag {
                work_item_id: 3,
                team: "P".into(),
                code: FlagCode::Stale,
                severity: poseidon_core::Severity::Warn,
                message: String::new(),
                tag: None,
            },
        ];
        let counts = count_by_code(&flags);
        assert_eq!(counts[0].tag, "stale");
        assert_eq!(counts[0].count, 2);
    }

    #[test]
    fn count_entity_flags_groups_by_code_descending() {
        let flag = |code: &str| EntityFlag {
            code: code.into(),
            severity: poseidon_core::Severity::Warn,
            message: String::new(),
        };
        let flags = [
            flag("stale-open"),
            flag("no-work-item"),
            flag("no-work-item"),
            flag("stale-draft"),
        ];
        let counts = count_entity_flags(flags.iter());
        // Most frequent first; ties broken alphabetically by code.
        assert_eq!(counts[0].tag, "no-work-item");
        assert_eq!(counts[0].count, 2);
        // The two singletons keep a stable (alphabetical) order.
        assert_eq!(counts[1].tag, "stale-draft");
        assert_eq!(counts[2].tag, "stale-open");
        assert!(count_entity_flags(std::iter::empty()).is_empty());
    }

    // ─────────────────────── Service logic tests ───────────────────────
    //
    // These build a real `Service` over an in-memory SQLite store and exercise the
    // read-side logic: the tag-suggestion merge in `work_items`, per-team
    // `evaluate_scoped`, owner isolation, and the AI suggestion run (with a stub
    // tagger injected into the per-owner cache).

    use poseidon_core::{RuleSet, TagKeywords, UserConfig};

    /// A `Service` over a fresh in-memory store, owner = `default`.
    async fn test_service() -> Service {
        let store = Store::connect_in_memory().await.unwrap();
        Service::new(PoseidonConfig::default(), store, std::env::temp_dir())
    }

    #[tokio::test]
    async fn catalog_resolves_product_from_linked_repo_and_config_overrides() {
        use poseidon_core::{CatalogConfig, UserConfig};
        let svc = test_service().await;

        // Owner config: a catalog alias (raw id -> slug) + a repo_tags OVERRIDE that
        // maps the same repo to a different product.
        let rules = RuleSet {
            catalog: Some(CatalogConfig {
                source: "csv".into(),
                product_aliases: [("widget-assistant--wa-".to_string(), "wa".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            }),
            repo_tags: vec![keyword("product:override", &["OverriddenRepo"])],
            ..Default::default()
        };
        svc.config
            .set_user_config(
                DEFAULT_OWNER,
                UserConfig {
                    rules,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Catalog: two repos -> the same product; one of them is ALSO in repo_tags.
        let ent = |repo: &str| CatalogEntity {
            repo: Some(repo.into()),
            product: Some("widget-assistant--wa-".into()),
            team: None,
            domain: None,
            kind: None,
        };
        svc.store
            .replace_catalog(
                DEFAULT_OWNER,
                &[ent("Contoso.Assistant"), ent("OverriddenRepo")],
            )
            .await
            .unwrap();

        // Item A links a catalog-only repo -> gets product:wa from the catalog.
        let mut a = work_item(1, "Platform", "A", &[]);
        a.linked_repos = vec!["Contoso.Assistant".into()];
        // Item B links the repo that repo_tags overrides -> config wins, no product:wa.
        let mut b = work_item(2, "Platform", "B", &[]);
        b.linked_repos = vec!["OverriddenRepo".into()];
        svc.store
            .upsert_work_items(DEFAULT_OWNER, &[a, b])
            .await
            .unwrap();

        let items = svc.work_items(None).await.unwrap();
        let a = items.iter().find(|i| i.id == 1).unwrap();
        let b = items.iter().find(|i| i.id == 2).unwrap();
        assert!(
            has_suggestion(a, "product:wa"),
            "catalog resolves product from linked repo"
        );
        assert!(
            has_suggestion(b, "product:override"),
            "config repo_tags fires"
        );
        assert!(
            !has_suggestion(b, "product:wa"),
            "config repo_tags OVERRIDES the catalog"
        );
    }

    #[tokio::test]
    async fn sync_catalog_csv_honours_configured_field_map() {
        use poseidon_core::{CatalogConfig, CatalogFieldMap, UserConfig};
        let svc = test_service().await;
        // Config points at NON-default column names, proving the field map drives it.
        let rules = RuleSet {
            catalog: Some(CatalogConfig {
                source: "csv".into(),
                field_map: Some(CatalogFieldMap {
                    product: "Svc".into(),
                    team: "Owner".into(),
                    repo_source: "Git".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        svc.config
            .set_user_config(
                DEFAULT_OWNER,
                UserConfig {
                    rules,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let csv =
            "Git,Owner,Svc\nhttps://dev.azure.com/contoso/P/_git/Contoso.Svc,team-a,widget--w-\n";
        let n = svc.sync_catalog_csv(csv).await.unwrap();
        assert_eq!(n, 1);
        let rows = svc.catalog().await.unwrap();
        assert_eq!(rows[0].repo.as_deref(), Some("Contoso.Svc"));
        assert_eq!(rows[0].product.as_deref(), Some("widget--w-"));
        assert_eq!(rows[0].team.as_deref(), Some("team-a"));
    }

    #[tokio::test]
    async fn sync_catalog_from_csv_source_stores_repo_rows() {
        use poseidon_providers::{CsvCatalog, FieldMap};
        let svc = test_service().await;
        let csv = "Title,Type,Owning Teams,Source,Description,Product,Framework,Language,Link\n\
                   Assistant API,API,team-alpha,https://dev.azure.com/contoso/P/_git/Contoso.Assistant,,widget-assistant--wa-,.NET,C#,\n";
        let source = CsvCatalog::new(csv, FieldMap::port_service_export());
        let n = svc.sync_catalog_from(&source).await.unwrap();
        assert_eq!(n, 1);
        let rows = svc.catalog().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo.as_deref(), Some("Contoso.Assistant"));
        assert_eq!(rows[0].product.as_deref(), Some("widget-assistant--wa-"));
    }

    /// A minimal work item; state `Active`, type `User Story`, no PRs.
    fn work_item(id: i64, team: &str, title: &str, tags: &[&str]) -> WorkItem {
        WorkItem {
            id,
            provider: "stub".into(),
            team: team.into(),
            title: title.into(),
            work_item_type: "User Story".into(),
            state: "Active".into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            assigned_to: None,
            created_at: Utc::now(),
            changed_at: Utc::now(),
            closed_at: None,
            iteration_path: None,
            story_points: None,
            description: None,
            url: String::new(),
            linked_pr_ids: Vec::new(),
            parent_id: None,
            linked_repos: Vec::new(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
        }
    }

    fn keyword(tag: &str, words: &[&str]) -> TagKeywords {
        TagKeywords {
            tag: tag.into(),
            keywords: words.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// True if the item carries a suggestion for `tag` (case-insensitive).
    fn has_suggestion(item: &WorkItem, tag: &str) -> bool {
        item.tag_suggestions
            .iter()
            .any(|s| s.tag.eq_ignore_ascii_case(tag))
    }

    #[tokio::test]
    async fn stored_audit_findings_surface_as_ai_audit_flags_in_scope() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        svc.store
            .replace_team_work_items(
                &owner,
                "Platform",
                &[work_item(1, "Platform", "Fix it", &[])],
            )
            .await
            .unwrap();
        // A finding for an item in scope surfaces as an ai_audit flag...
        svc.store
            .set_ai_audit(
                &owner,
                "Platform",
                1,
                &[("unclear".into(), "No component named".into())],
            )
            .await
            .unwrap();
        // ...while a finding for an id not in this read's items is ignored (no ghost flag).
        svc.store
            .set_ai_audit(
                &owner,
                "Platform",
                999,
                &[("bad_data".into(), "orphan".into())],
            )
            .await
            .unwrap();

        let flags = svc.flags(None).await.unwrap();
        let audit: Vec<&Flag> = flags
            .iter()
            .filter(|f| f.code == FlagCode::AiAudit)
            .collect();
        assert_eq!(audit.len(), 1, "only the in-scope finding surfaces");
        let f = audit[0];
        assert_eq!(f.work_item_id, 1);
        assert_eq!(f.team, "Platform");
        assert_eq!(f.severity, Severity::Warn);
        // The message carries the human kind label + the model's detail.
        assert!(f.message.contains("unclear"), "{}", f.message);
        assert!(f.message.contains("No component named"), "{}", f.message);
        assert!(
            !flags.iter().any(|f| f.work_item_id == 999),
            "orphan finding excluded"
        );
    }

    #[tokio::test]
    async fn work_items_merges_keyword_and_ai_suggestions_deduped() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // Default ruleset suggests `area:data` for anything mentioning "database".
        let cfg = UserConfig {
            rules: RuleSet {
                tag_keywords: vec![keyword("area:data", &["database"])],
                ..Default::default()
            },
            ..Default::default()
        };
        svc.config.set_user_config(&owner, cfg).await.unwrap();
        svc.store
            .upsert_work_items(
                &owner,
                &[work_item(1, "Alpha", "Database migration work", &[])],
            )
            .await
            .unwrap();
        // Stored AI suggestions: one novel, one that case-dupes the keyword hit.
        svc.store
            .set_ai_suggestions(
                &owner,
                "Alpha",
                1,
                &[
                    ("priority:high".into(), "ai".into()),
                    ("AREA:DATA".into(), "ai".into()),
                ],
            )
            .await
            .unwrap();

        let items = svc.work_items(None).await.unwrap();
        let it = &items[0];
        // area:data (keyword) + priority:high (AI); AREA:DATA de-duped against the
        // keyword hit, so exactly two suggestions.
        assert_eq!(it.tag_suggestions.len(), 2, "{:?}", it.tag_suggestions);
        assert!(has_suggestion(it, "area:data"));
        assert!(has_suggestion(it, "priority:high"));
        // The kept `area:data` is the KEYWORD one (its reason is the matched word),
        // proving the keyword suggestion wins over the case-dupe AI one.
        let area = it
            .tag_suggestions
            .iter()
            .find(|s| s.tag == "area:data")
            .unwrap();
        assert_eq!(area.reasons, vec!["database".to_string()]);
    }

    #[tokio::test]
    async fn work_items_drops_ai_suggestion_already_applied() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // No keywords: isolate the AI-merge branch. Item already carries area:data.
        svc.store
            .upsert_work_items(
                &owner,
                &[work_item(2, "Alpha", "Some ticket", &["area:data"])],
            )
            .await
            .unwrap();
        svc.store
            .set_ai_suggestions(
                &owner,
                "Alpha",
                2,
                &[
                    ("area:data".into(), "ai".into()),
                    ("type:bug".into(), "ai".into()),
                ],
            )
            .await
            .unwrap();

        let items = svc.work_items(None).await.unwrap();
        let it = &items[0];
        // area:data is already applied -> dropped; only type:bug survives.
        assert_eq!(it.tag_suggestions.len(), 1, "{:?}", it.tag_suggestions);
        assert!(has_suggestion(it, "type:bug"));
        assert!(!has_suggestion(it, "area:data"));
    }

    #[tokio::test]
    async fn work_items_use_description_toggle_gates_body() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        let cfg = UserConfig {
            rules: RuleSet {
                tag_keywords: vec![keyword("area:payments", &["payment"])],
                ..Default::default()
            },
            ..Default::default()
        };
        svc.config.set_user_config(&owner, cfg).await.unwrap();
        // The trigger word is ONLY in the description, not the title.
        let mut item = work_item(3, "Alpha", "Fix login", &[]);
        item.description = Some("Involves the payment gateway".into());
        svc.store.upsert_work_items(&owner, &[item]).await.unwrap();

        // Default (use_description = true) reads the body -> suggests area:payments.
        let items = svc.work_items(None).await.unwrap();
        assert!(has_suggestion(&items[0], "area:payments"));

        // Opt out -> the body is no longer consulted, so the suggestion vanishes.
        svc.set_tag_use_description(false).await.unwrap();
        let items = svc.work_items(None).await.unwrap();
        assert!(!has_suggestion(&items[0], "area:payments"));
    }

    #[tokio::test]
    async fn evaluate_scoped_judges_each_team_by_its_effective_rules() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // Instance default disallows the `wip` tag. Beta overrides with an empty
        // ruleset (disallows nothing). Alpha inherits the default.
        let cfg = UserConfig {
            teams: vec![
                TeamConfig {
                    name: "Alpha".into(),
                    provider: poseidon_core::ProviderKind::Stub,
                    organization: "https://stub.example".into(),
                    project: "Alpha".into(),
                    tenant: None,
                    area_path: None,
                    area_path_strict: false,
                    auth: Default::default(),
                    wiql: None,
                    pipeline_ids: vec![],
                    rules: None,
                },
                TeamConfig {
                    name: "Beta".into(),
                    provider: poseidon_core::ProviderKind::Stub,
                    organization: "https://stub.example".into(),
                    project: "Beta".into(),
                    tenant: None,
                    area_path: None,
                    area_path_strict: false,
                    auth: Default::default(),
                    wiql: None,
                    pipeline_ids: vec![],
                    rules: Some(RuleSet::default()),
                },
            ],
            rules: RuleSet {
                disallowed_tags: vec!["wip".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        svc.config.set_user_config(&owner, cfg).await.unwrap();

        let items = vec![
            work_item(1, "Alpha", "a", &["wip"]), // default rules -> flagged
            work_item(2, "Beta", "b", &["wip"]),  // override -> not flagged
            work_item(3, "Gamma", "g", &["wip"]), // team not in config -> default -> flagged
        ];
        let flags = svc.evaluate_scoped(&items).await;
        let disallowed: Vec<i64> = flags
            .iter()
            .filter(|f| f.code == FlagCode::DisallowedTag)
            .map(|f| f.work_item_id)
            .collect();
        assert!(disallowed.contains(&1), "Alpha inherits the disallow rule");
        assert!(!disallowed.contains(&2), "Beta's override permits wip");
        assert!(
            disallowed.contains(&3),
            "unknown team falls back to the default"
        );
    }

    #[tokio::test]
    async fn owner_scoping_isolates_items_suggestions_and_settings() {
        let svc = test_service().await;
        let a = svc.with_owner("alice@example.com");
        let b = svc.with_owner("bob@example.com");

        // Alice's data.
        a.store
            .upsert_work_items(a.owner(), &[work_item(1, "Alpha", "alice item", &[])])
            .await
            .unwrap();
        a.store
            .set_ai_suggestions(a.owner(), "Alpha", 1, &[("type:bug".into(), "ai".into())])
            .await
            .unwrap();
        a.set_tag_use_description(false).await.unwrap();

        // Bob sees none of it.
        assert!(b.work_items(None).await.unwrap().is_empty(), "no item leak");
        assert!(
            b.store
                .ai_suggestions(b.owner(), None)
                .await
                .unwrap()
                .is_empty(),
            "no suggestion leak"
        );
        assert!(
            b.tag_use_description().await,
            "Bob keeps the default toggle"
        );

        // Alice still has her own.
        assert_eq!(a.work_items(None).await.unwrap().len(), 1);
        assert!(!a.tag_use_description().await);
    }

    /// A deterministic tagger: suggests the FIRST allowed tag (if any), so the run's
    /// counts are predictable without a network model.
    struct StubTagger;
    #[async_trait::async_trait]
    impl poseidon_ai::AiTagger for StubTagger {
        async fn suggest(
            &self,
            _item: &poseidon_ai::TaggerInput,
            allowed: &[String],
            _required: &[String],
            _hints: &poseidon_ai::TagHints,
            _background: &str,
        ) -> Result<Vec<poseidon_core::TagSuggestion>, poseidon_ai::AiError> {
            Ok(allowed
                .first()
                .map(|t| poseidon_core::TagSuggestion {
                    tag: t.clone(),
                    reasons: vec!["stub".into()],
                    replaces: None,
                })
                .into_iter()
                .collect())
        }
    }

    /// Inject a stub tagger into the owner's cache so `ai_tagger()` resolves it.
    fn install_stub_tagger(svc: &Service) {
        svc.ai.write().unwrap().insert(
            svc.owner.clone(),
            Some(Arc::new(StubTagger) as Arc<dyn poseidon_ai::AiTagger>),
        );
    }

    #[tokio::test]
    async fn generate_tag_suggestions_scopes_by_ids_and_counts() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // A concrete approved set (the default rules apply to every team here).
        let cfg = UserConfig {
            rules: RuleSet {
                allowed_tags: vec!["area:data".into(), "type:bug".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        svc.config.set_user_config(&owner, cfg).await.unwrap();
        svc.store
            .upsert_work_items(
                &owner,
                &[
                    work_item(1, "Alpha", "one", &[]),
                    work_item(2, "Alpha", "two", &[]),
                    work_item(3, "Alpha", "three", &[]),
                ],
            )
            .await
            .unwrap();
        install_stub_tagger(&svc);
        assert!(svc.ai_enabled().await);

        // Scope to ids {1, 3}: item 2 is never considered.
        let summary = svc
            .generate_tag_suggestions_with(None, Some(&[1, 3]), |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(summary.considered, 2);
        assert_eq!(summary.with_suggestions, 2);
        assert_eq!(summary.suggestions, 2, "one suggestion per considered item");

        // Persisted only for the scoped ids.
        let stored = svc.store.ai_suggestions(&owner, None).await.unwrap();
        assert!(stored.contains_key(&1));
        assert!(stored.contains_key(&3));
        assert!(!stored.contains_key(&2), "item 2 was out of the id scope");
    }

    #[tokio::test]
    async fn generate_tag_suggestions_skips_team_without_concrete_set() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // Only a wildcard allowed tag and nothing in use behind it -> no concrete
        // candidates -> item is skipped.
        let cfg = UserConfig {
            rules: RuleSet {
                allowed_tags: vec!["area:*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        svc.config.set_user_config(&owner, cfg).await.unwrap();
        svc.store
            .upsert_work_items(&owner, &[work_item(1, "Alpha", "one", &[])])
            .await
            .unwrap();
        install_stub_tagger(&svc);

        let summary = svc
            .generate_tag_suggestions_with(None, None, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(
            summary.considered, 0,
            "no concrete approved set -> nothing considered"
        );
        assert_eq!(summary.suggestions, 0);
    }

    #[tokio::test]
    async fn tag_use_description_round_trips_and_defaults_true() {
        let svc = test_service().await;
        // Unset -> defaults to true (richer signal).
        assert!(svc.tag_use_description().await);
        svc.set_tag_use_description(false).await.unwrap();
        assert!(!svc.tag_use_description().await);
        svc.set_tag_use_description(true).await.unwrap();
        assert!(svc.tag_use_description().await);
    }

    #[tokio::test]
    async fn reset_llm_config_clears_stored_registry_and_tagger_cache() {
        let svc = test_service().await;
        let owner = svc.owner.clone();
        // Persist a marker registry + prime the tagger cache, then reset.
        svc.store
            .set_meta(&owner, "llm_config", "{\"integrations\":[]}")
            .await
            .unwrap();
        install_stub_tagger(&svc);
        assert!(svc.ai.read().unwrap().contains_key(&owner));

        svc.reset_llm_config().await.unwrap();
        assert!(
            svc.store
                .get_meta(&owner, "llm_config")
                .await
                .unwrap()
                .is_none(),
            "stored registry cleared"
        );
        assert!(
            !svc.ai.read().unwrap().contains_key(&owner),
            "tagger cache invalidated"
        );
    }
}
