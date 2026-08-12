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
use poseiden_core::{TagSuggestion, WorkItem};
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
    ) -> Result<Vec<TagSuggestion>, AiError>;
}

/// Instance-level AI config: which backend suggests tags. Persisted (set via
/// onboarding / settings) or seeded from env. Serialisable so it round-trips
/// through the store and the config API.
///
///   mode = "online"  -> a hosted provider (Claude / ChatGPT / Gemini, or a
///                       custom OpenAI-compatible endpoint) + an API key.
///   mode = "offline" -> a small model POSEIDEN downloads and runs in-process
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
    /// Back-compat: `POSEIDEN_AI_ENDPOINT`/`_MODEL`/`_API_KEY` map to a custom
    /// online provider, so an env-configured deployment (e.g. the bundled Ollama
    /// via the chart) keeps working. Otherwise off.
    pub fn from_env() -> Self {
        let endpoint = env_nonempty("POSEIDEN_AI_ENDPOINT");
        let model = env_nonempty("POSEIDEN_AI_MODEL");
        if endpoint.is_some() && model.is_some() {
            Self {
                mode: "online".to_string(),
                provider: Some("custom".to_string()),
                endpoint,
                model,
                api_key: env_nonempty("POSEIDEN_AI_API_KEY"),
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
    let by_vram = |v: u32| {
        if v >= 12000 {
            "qwen2.5-7b"
        } else if v >= 7000 {
            "qwen2.5-3b"
        } else if v >= 4000 {
            "qwen2.5-1.5b"
        } else {
            "qwen2.5-0.5b"
        }
    };
    match kind {
        "webgpu" => match caps.vram_mb {
            Some(v) => by_vram(v),
            None => "qwen2.5-3b", // safe strong default; benchmark tunes higher
        },
        "offline" if device == "gpu" => caps.vram_mb.map(by_vram).unwrap_or("qwen2.5-3b"),
        "offline" => match caps.cpu_cores {
            Some(c) if c >= 8 => "qwen2.5-1.5b",
            _ => "qwen2.5-0.5b",
        },
        _ => "qwen2.5-0.5b",
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
                offline("local-gpu", "On-device GPU (CUDA)", "qwen2.5-1.5b", "gpu"),
                LlmIntegration {
                    id: "webgpu".to_string(),
                    name: "In-browser (WebGPU)".to_string(),
                    kind: "webgpu".to_string(),
                    offline_model: Some("qwen2.5-1.5b".to_string()),
                    ..Default::default()
                },
                offline("local-cpu", "On-device CPU", "qwen2.5-0.5b", "cpu"),
                LlmIntegration {
                    id: "ollama".to_string(),
                    name: "Local Ollama".to_string(),
                    kind: "online".to_string(),
                    provider: Some("custom".to_string()),
                    endpoint: Some("http://localhost:11434/v1/chat/completions".to_string()),
                    model: Some("qwen2.5:1.5b".to_string()),
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

/// Hard cap on suggestions per item, enforced after validation regardless of what
/// the model returns - a backstop for over-eager models that ignore "at most 3".
pub(crate) const MAX_SUGGESTIONS: usize = 3;

/// Match a tag against a required-tag pattern: trailing `*` is a prefix wildcard,
/// otherwise exact - both case-insensitive. (poseiden-ai doesn't depend on
/// poseiden-rules, so this mirrors `poseiden_rules::tag_matches` in miniature.)
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

    let mut out = format!(
        "Work item:\n- Title: {}\n- Type: {}{}\n- Current tags: {}\n",
        input.title, input.work_item_type, description, current
    );
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
                { "role": "user", "content": build_prompt(item, allowed, required, hints) },
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
        let allowed: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
        let raw = r#"{"tags":["t0","t1","t2","t3","t4","t5"],"rationale":"dumped everything"}"#;
        assert_eq!(parse_suggestions(raw, &allowed).len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn resolve_picks_first_platform_compatible_in_priority_order() {
        let mut gpu = integ("gpu", "offline");
        gpu.device = "gpu".into();
        gpu.offline_model = Some("qwen2.5-0.5b".into());
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
        // WebGPU with no VRAM number -> safe strong default (benchmark tunes higher).
        assert_eq!(recommend_model("webgpu", "", &none), "qwen2.5-3b");
        // WebGPU with a real big VRAM number -> the top tier.
        let big = PlatformCaps {
            vram_mb: Some(16000),
            ..Default::default()
        };
        assert_eq!(recommend_model("webgpu", "", &big), "qwen2.5-7b");
        // CPU-only scales by cores.
        let many = PlatformCaps {
            cpu_cores: Some(16),
            ..Default::default()
        };
        let few = PlatformCaps {
            cpu_cores: Some(2),
            ..Default::default()
        };
        assert_eq!(recommend_model("offline", "cpu", &many), "qwen2.5-1.5b");
        assert_eq!(recommend_model("offline", "cpu", &few), "qwen2.5-0.5b");
    }

    #[test]
    fn seeded_for_sizes_local_models_and_marks_auto() {
        // A beefy WebGPU box: the webgpu entry gets bumped to 7B; ordering unchanged.
        let caps = PlatformCaps {
            webgpu: true,
            vram_mb: Some(16000),
            cpu_cores: Some(16),
            ..Default::default()
        };
        let cfg = LlmConfig::seeded_for(&caps);
        assert!(cfg.auto, "auto-configured registries are flagged");
        let by = |id: &str| cfg.integrations.iter().find(|i| i.id == id).unwrap();
        assert_eq!(by("webgpu").offline_model.as_deref(), Some("qwen2.5-7b"));
        assert_eq!(
            by("local-cpu").offline_model.as_deref(),
            Some("qwen2.5-1.5b")
        );
        // Same integrations, same order as the plain catalog.
        let plain = LlmConfig::seeded();
        assert_eq!(
            cfg.integrations.iter().map(|i| &i.id).collect::<Vec<_>>(),
            plain.integrations.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn webgpu_is_only_compatible_on_a_webgpu_platform() {
        let mut wg = integ("wg", "webgpu");
        wg.model = Some("qwen2.5-0.5b-q4f16".into());
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
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default());
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
        assert!(build_prompt(&input, &allowed(), &[], &Default::default())
            .contains("- Current tags: (none)"));
    }

    #[test]
    fn build_prompt_omits_description_when_absent_or_blank() {
        let none = TaggerInput::from(&work_item("t", "Task", &[], None));
        assert!(!build_prompt(&none, &allowed(), &[], &Default::default()).contains("Description:"));
        // A whitespace-only body is treated as absent (trimmed, then filtered out).
        let blank = TaggerInput::from(&work_item("t", "Task", &[], Some("   \n\t ")));
        assert!(
            !build_prompt(&blank, &allowed(), &[], &Default::default()).contains("Description:")
        );
    }

    #[test]
    fn build_prompt_includes_description_when_present() {
        let input = TaggerInput::from(&work_item("t", "Task", &[], Some("crashes on retry")));
        assert!(build_prompt(&input, &allowed(), &[], &Default::default())
            .contains("- Description: crashes on retry"));
    }

    #[test]
    fn build_prompt_truncates_a_long_description_to_the_cap() {
        // A body over the cap is cut to MAX_DESC_CHARS chars. Use a marker char the
        // title/type/tags never contain so the count is unambiguous.
        let body = "Z".repeat(MAX_DESC_CHARS + 500);
        let input = TaggerInput::from(&work_item("t", "Task", &[], Some(&body)));
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default());
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
        let prompt = build_prompt(&input, &allowed, &required, &Default::default());

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
        let prompt = build_prompt(&input, &allowed, &required, &hints);
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
        let prompt = build_prompt(&input, &allowed, &["area:*".to_string()], &hints);
        // Only MAX_HINTS_PER_TAG examples are shown.
        assert_eq!(prompt.matches("kw").count(), MAX_HINTS_PER_TAG);
    }

    #[test]
    fn build_prompt_without_required_matches_the_plain_allowed_layout() {
        // No required patterns -> the old "ALLOWED tags:" layout, no required block.
        let input = TaggerInput::from(&work_item("t", "Task", &[], None));
        let prompt = build_prompt(&input, &allowed(), &[], &Default::default());
        assert!(prompt.contains("ALLOWED tags:"));
        assert!(!prompt.contains("REQUIRED categories"));
    }
}
