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

/// A small quantized model POSEIDON can download and run in-process (offline).
/// `repo`/`file` point at a GGUF on Hugging Face; `size_mb` is the download.
/// `tokenizer_repo` is the *base* (non-GGUF) model repo: GGUF repos ship the
/// weights but not `tokenizer.json`, so it must be fetched from the base repo.
#[derive(Debug, Clone, Serialize)]
pub struct OfflineModel {
    pub id: &'static str,
    pub label: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub tokenizer_repo: &'static str,
    pub size_mb: u32,
}

// All Qwen2.5 GGUF today: the embedded engine loads the Qwen2 architecture. Other
// families are added alongside their candle loader.
pub const OFFLINE_MODELS: &[OfflineModel] = &[
    OfflineModel {
        id: "qwen2.5-0.5b",
        label: "Qwen2.5 0.5B - fastest, lightest (~400 MB)",
        repo: "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
        file: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-0.5B-Instruct",
        size_mb: 400,
    },
    OfflineModel {
        id: "qwen2.5-1.5b",
        label: "Qwen2.5 1.5B - balanced (~1 GB)",
        repo: "Qwen/Qwen2.5-1.5B-Instruct-GGUF",
        file: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-1.5B-Instruct",
        size_mb: 1000,
    },
    OfflineModel {
        id: "qwen2.5-3b",
        label: "Qwen2.5 3B - more accurate (~2 GB; GPU/WebGPU)",
        repo: "Qwen/Qwen2.5-3B-Instruct-GGUF",
        file: "qwen2.5-3b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-3B-Instruct",
        size_mb: 2000,
    },
    OfflineModel {
        id: "qwen2.5-7b",
        label: "Qwen2.5 7B - best tagging accuracy (~4.7 GB; needs a GPU/WebGPU)",
        repo: "Qwen/Qwen2.5-7B-Instruct-GGUF",
        file: "qwen2.5-7b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-7B-Instruct",
        size_mb: 4700,
    },
];

pub fn online_provider(id: &str) -> Option<&'static OnlineProvider> {
    ONLINE_PROVIDERS.iter().find(|p| p.id == id)
}

pub fn offline_model(id: &str) -> Option<&'static OfflineModel> {
    OFFLINE_MODELS.iter().find(|m| m.id == id)
}
