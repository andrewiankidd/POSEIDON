//! Curated AI backends the onboarding / settings UI offers: hosted providers
//! (bring an API key) and small offline models (downloaded + run in-process, no
//! Ollama, no server). Kept as data so the UI can render the choices and the
//! config can validate against them.

use serde::Serialize;

/// A hosted, OpenAI-compatible provider. The user supplies an API key; POSEIDON
/// talks to `endpoint` (all three expose an OpenAI-compatible chat/completions API).
#[derive(Debug, Clone, Serialize)]
pub struct OnlineProvider {
    pub id: &'static str,
    pub label: &'static str,
    pub endpoint: &'static str,
    pub default_model: &'static str,
    /// Where to get an API key (shown in the UI).
    pub key_url: &'static str,
}

pub const ONLINE_PROVIDERS: &[OnlineProvider] = &[
    OnlineProvider {
        id: "anthropic",
        label: "Claude (Anthropic)",
        endpoint: "https://api.anthropic.com/v1/chat/completions",
        default_model: "claude-3-5-haiku-latest",
        key_url: "https://console.anthropic.com/settings/keys",
    },
    OnlineProvider {
        id: "openai",
        label: "ChatGPT (OpenAI)",
        endpoint: "https://api.openai.com/v1/chat/completions",
        default_model: "gpt-4o-mini",
        key_url: "https://platform.openai.com/api-keys",
    },
    OnlineProvider {
        id: "gemini",
        label: "Gemini (Google)",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
        default_model: "gemini-1.5-flash",
        key_url: "https://aistudio.google.com/apikey",
    },
];

/// A small quantized model POSEIDON can download and run offline. THIS IS THE SINGLE
/// SOURCE OF TRUTH for the model catalog - the web-llm (WebGPU) id, the GGUF (embedded)
/// coordinates, the VRAM footprint and auto-eligibility all live here, so swapping or
/// adding a model family is a one-place edit: `recommend_model` sizes from it, and the
/// frontend receives it (serialized) and builds its WebGPU id map + fallback ladder from
/// it - no ids duplicated in JS.
///
/// - `repo`/`file`/`tokenizer_repo`: the GGUF + base repo for the in-process (candle)
///   engine. `size_mb` is the download.
/// - `webgpu_id`: the web-llm/MLC prebuilt model id ("" if not WebGPU-runnable).
/// - `min_vram_mb`: VRAM needed to run it (weights + KV/activation headroom). The picker
///   selects the largest fitting model; the WebGPU probe measures allocatable VRAM.
/// - `auto`: whether autotune may pick it. A big model (e.g. 8B) can be `false` so it's
///   only ever hand-picked, keeping the auto default at the best quality/speed balance.
/// - `default`: the ONE balanced model picked when the platform's VRAM can't be measured
///   (exactly one entry should set it). Read via [`default_auto_model`] so "what we pick
///   when we don't know the hardware" lives here, not as a literal id scattered in code.
#[derive(Debug, Clone, Serialize)]
pub struct OfflineModel {
    pub id: &'static str,
    pub label: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub tokenizer_repo: &'static str,
    pub size_mb: u32,
    pub webgpu_id: &'static str,
    pub min_vram_mb: u32,
    pub auto: bool,
    pub default: bool,
}

// Qwen3 (2025): a generational step over Qwen2.5 - a 4B roughly matches Qwen2.5-7B on
// instruction-following while being ~2× faster in WebGPU, so it's the recommended default.
// The 8B is present for hand-pick (auto:false). NOTE: Qwen3 has a "thinking" mode that the
// frontend suppresses (/no_think) for tagging/drafting so it doesn't burn tokens reasoning.
// GGUF coords are for the embedded (candle) engine - unverified against candle's Qwen3
// support; the WebGPU path (webgpu_id -> web-llm) is the tested one. Ordered small→large.
pub const OFFLINE_MODELS: &[OfflineModel] = &[
    OfflineModel {
        id: "qwen3-0.6b",
        label: "Qwen3 0.6B - fastest, lightest (~600 MB)",
        repo: "Qwen/Qwen3-0.6B-GGUF",
        file: "Qwen3-0.6B-Q4_K_M.gguf",
        tokenizer_repo: "Qwen/Qwen3-0.6B",
        size_mb: 600,
        webgpu_id: "Qwen3-0.6B-q4f16_1-MLC",
        min_vram_mb: 1000,
        auto: true,
        default: false,
    },
    OfflineModel {
        id: "qwen3-1.7b",
        label: "Qwen3 1.7B - balanced (~1.2 GB)",
        repo: "Qwen/Qwen3-1.7B-GGUF",
        file: "Qwen3-1.7B-Q4_K_M.gguf",
        tokenizer_repo: "Qwen/Qwen3-1.7B",
        size_mb: 1200,
        webgpu_id: "Qwen3-1.7B-q4f16_1-MLC",
        min_vram_mb: 1600,
        auto: true,
        default: true,
    },
    OfflineModel {
        id: "qwen3-4b",
        label: "Qwen3 4B - recommended: ~7B-class quality, ~2× faster (~2.5 GB; GPU/WebGPU)",
        repo: "Qwen/Qwen3-4B-GGUF",
        file: "Qwen3-4B-Q4_K_M.gguf",
        tokenizer_repo: "Qwen/Qwen3-4B",
        size_mb: 2500,
        webgpu_id: "Qwen3-4B-q4f16_1-MLC",
        min_vram_mb: 3000,
        auto: true,
        default: false,
    },
    OfflineModel {
        id: "qwen3-8b",
        label: "Qwen3 8B - highest quality, slower (~5 GB; hand-pick, needs a big GPU)",
        repo: "Qwen/Qwen3-8B-GGUF",
        file: "Qwen3-8B-Q4_K_M.gguf",
        tokenizer_repo: "Qwen/Qwen3-8B",
        size_mb: 5000,
        webgpu_id: "Qwen3-8B-q4f16_1-MLC",
        min_vram_mb: 6000,
        auto: false,
        default: false,
    },
];

pub fn online_provider(id: &str) -> Option<&'static OnlineProvider> {
    ONLINE_PROVIDERS.iter().find(|p| p.id == id)
}

pub fn offline_model(id: &str) -> Option<&'static OfflineModel> {
    OFFLINE_MODELS.iter().find(|m| m.id == id)
}

/// The smallest auto-eligible model - the universal fallback that runs anywhere. Used
/// wherever code needs "the safest local model" without naming an id.
pub fn smallest_auto_model() -> &'static OfflineModel {
    OFFLINE_MODELS
        .iter()
        .filter(|m| m.auto)
        .min_by_key(|m| m.min_vram_mb)
        .unwrap_or(&OFFLINE_MODELS[0])
}

/// The balanced default to use when the platform's VRAM can't be measured: the catalog
/// entry flagged `default`, else the smallest auto model. The single place that answers
/// "which model when we don't know the hardware" - no literal id anywhere else.
pub fn default_auto_model() -> &'static OfflineModel {
    OFFLINE_MODELS
        .iter()
        .find(|m| m.default)
        .unwrap_or_else(|| smallest_auto_model())
}
