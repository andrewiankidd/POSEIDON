//! Offline, in-process tag suggestion via a small quantized Qwen2.5 model
//! (candle). On first use it downloads the preset GGUF + tokenizer from Hugging
//! Face (cached under the HF cache dir), loads them once, and generates locally -
//! no Ollama, no server, no network at inference time. CPU by default.
//!
//! Compilation validates the candle API usage; output quality (does a 0.5-1.5B
//! model tag well) needs a real model run to confirm and tune.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::quantized_qwen2::ModelWeights;
use poseiden_core::TagSuggestion;
use tokenizers::Tokenizer;

use crate::presets::OfflineModel;
use crate::{build_prompt, parse_suggestions, AiError, AiTagger, TaggerInput, SYSTEM_PROMPT};

/// Cap the reply length - the JSON tag object is short.
const MAX_NEW_TOKENS: usize = 200;

/// An offline tagger backed by a downloaded quantized model. Loaded lazily on the
/// first `suggest` (the download can be hundreds of MB) and reused after.
/// One cached model's slot: an optionally-loaded model behind a mutex that
/// serialises inference on it, shared via `Arc` across owners.
type ModelSlot = Arc<Mutex<Option<Loaded>>>;
/// Process-wide model cache, keyed by model id.
type ModelCache = Mutex<HashMap<String, ModelSlot>>;

/// Loaded models shared process-wide by model id, so multiple owners choosing the
/// same offline model share one in-memory copy (a GGUF is ~GB) instead of loading
/// it per owner. The inner mutex serialises inference on a given model.
static MODEL_CACHE: OnceLock<ModelCache> = OnceLock::new();

fn model_slot(id: &str) -> ModelSlot {
    let cache = MODEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    map.entry(id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

pub struct EmbeddedTagger {
    preset: &'static OfflineModel,
    /// The shared, lazily-loaded model for this id (one copy across all owners).
    slot: Arc<Mutex<Option<Loaded>>>,
}

struct Loaded {
    model: ModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    eos: u32,
}

impl EmbeddedTagger {
    pub fn new(preset: &'static OfflineModel) -> Self {
        Self {
            preset,
            slot: model_slot(preset.id),
        }
    }
}

#[async_trait]
impl AiTagger for EmbeddedTagger {
    async fn suggest(
        &self,
        item: &TaggerInput,
        allowed: &[String],
        required: &[String],
        hints: &crate::TagHints,
    ) -> Result<Vec<TagSuggestion>, AiError> {
        if allowed.is_empty() {
            return Ok(vec![]);
        }
        let prompt = chat_prompt(&build_prompt(item, allowed, required, hints));
        let allowed = allowed.to_vec();
        let current = item.current_tags.clone();
        let preset = self.preset;
        let slot = self.slot.clone();

        // Inference is blocking CPU work - keep it off the async runtime.
        let text = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut guard = slot.lock().map_err(|_| "model lock poisoned".to_string())?;
            if guard.is_none() {
                *guard = Some(load(preset)?);
            }
            let loaded = guard.as_mut().unwrap();
            generate(loaded, &prompt)
        })
        .await
        .map_err(|e| AiError::Http(format!("inference task failed: {e}")))?
        .map_err(AiError::Http)?;

        let mut suggestions = parse_suggestions(&text, &allowed);
        let have: std::collections::HashSet<String> =
            current.iter().map(|t| t.to_lowercase()).collect();
        suggestions.retain(|s| !have.contains(&s.tag.to_lowercase()));
        Ok(suggestions)
    }
}

/// Wrap the system + user messages in Qwen2.5's chat template.
fn chat_prompt(user: &str) -> String {
    format!(
        "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
    )
}

/// Whether a CUDA GPU is actually usable: only in a `--features cuda` build AND
/// with a live device. Drives platform compatibility for GPU-offline integrations.
pub(crate) fn cuda_available() -> bool {
    #[cfg(feature = "cuda")]
    {
        Device::cuda_if_available(0)
            .map(|d| d.is_cuda())
            .unwrap_or(false)
    }
    #[cfg(not(feature = "cuda"))]
    {
        false
    }
}

/// Download (cached) + load the model and tokenizer.
fn load(preset: &OfflineModel) -> Result<Loaded, String> {
    use hf_hub::api::sync::Api;
    let api = Api::new().map_err(|e| format!("hf-hub init: {e}"))?;
    let repo = api.model(preset.repo.to_string());
    tracing::info!(
        model = preset.id,
        "downloading / loading offline model (first use can take a while)"
    );
    let gguf_path = repo
        .get(preset.file)
        .map_err(|e| format!("download model gguf: {e}"))?;
    // tokenizer.json lives in the base model repo, not the GGUF repo (which 404s).
    // We fetch it with a plain HTTP GET rather than hf-hub: hf-hub 0.3's sync
    // downloader chokes on that repo's redirect ("RelativeUrlWithoutBase") even
    // though the URL is reachable. reqwest::blocking is fine here - load() already
    // runs inside spawn_blocking, off the async runtime.
    let tok_url = format!(
        "https://huggingface.co/{}/resolve/main/tokenizer.json",
        preset.tokenizer_repo
    );
    let tok_bytes = reqwest::blocking::Client::new()
        .get(&tok_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("download tokenizer {tok_url}: {e}"))?;

    // GPU when built with `--features cuda` and an NVIDIA device is present;
    // plain CPU otherwise (the default build - byte-for-byte the old behaviour).
    // cuda_if_available falls back to CPU on any error, so a CUDA build on a
    // machine with no GPU still runs (just slowly).
    #[cfg(feature = "cuda")]
    let device = Device::cuda_if_available(0).unwrap_or(Device::Cpu);
    #[cfg(not(feature = "cuda"))]
    let device = Device::Cpu;
    tracing::info!(
        gpu = device.is_cuda(),
        model = preset.id,
        "embedded model device"
    );
    let mut file = std::fs::File::open(&gguf_path).map_err(|e| format!("open gguf: {e}"))?;
    let content = gguf_file::Content::read(&mut file).map_err(|e| format!("read gguf: {e}"))?;
    let model = ModelWeights::from_gguf(content, &mut file, &device)
        .map_err(|e| format!("load model: {e}"))?;
    let tokenizer =
        Tokenizer::from_bytes(&tok_bytes).map_err(|e| format!("load tokenizer: {e}"))?;
    let eos = tokenizer.token_to_id("<|im_end|>").unwrap_or(151645);
    tracing::info!(model = preset.id, "offline model ready");
    Ok(Loaded {
        model,
        tokenizer,
        device,
        eos,
    })
}

/// Greedy generation until EOS or the token cap. Reuses the model's KV cache by
/// starting each sequence at index 0 (a new prompt overwrites earlier positions
/// and only attends within its own length).
fn generate(loaded: &mut Loaded, prompt: &str) -> Result<String, String> {
    let cerr = |e: candle_core::Error| e.to_string();
    let device = loaded.device.clone();

    let encoding = loaded
        .tokenizer
        .encode(prompt, true)
        .map_err(|e| e.to_string())?;
    let tokens = encoding.get_ids();
    if tokens.is_empty() {
        return Ok(String::new());
    }
    let mut logits_processor = LogitsProcessor::new(42, Some(0.0), None);

    // Prefill: feed the whole prompt, get the next token.
    let input = Tensor::new(tokens, &device)
        .map_err(cerr)?
        .unsqueeze(0)
        .map_err(cerr)?;
    let logits = loaded.model.forward(&input, 0).map_err(cerr)?;
    let logits = logits.squeeze(0).map_err(cerr)?;
    let mut next = logits_processor.sample(&logits).map_err(cerr)?;

    let mut out: Vec<u32> = Vec::new();
    for i in 0..MAX_NEW_TOKENS {
        if next == loaded.eos {
            break;
        }
        out.push(next);
        let input = Tensor::new(&[next], &device)
            .map_err(cerr)?
            .unsqueeze(0)
            .map_err(cerr)?;
        let logits = loaded
            .model
            .forward(&input, tokens.len() + i)
            .map_err(cerr)?;
        let logits = logits.squeeze(0).map_err(cerr)?;
        next = logits_processor.sample(&logits).map_err(cerr)?;
    }
    loaded
        .tokenizer
        .decode(&out, true)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::offline_model;

    // Opt-in integration test for the REAL embedded runtime: it downloads the
    // model (~400MB) + tokenizer and runs CPU inference, so it is `#[ignore]`d and
    // must be asked for:
    //   cargo test -p poseiden-ai -- --ignored embedded_tagger
    // This is the regression net for the class of bugs found by dogfooding (the
    // read-only cache, the wrong tokenizer repo, the hf-hub URL quirk): a green run
    // proves load() + generate() + parse_suggestions() work end to end. The 0.5B
    // model is weak, so it may return no suggestions - that's allowed; what must
    // hold is that it loads + runs without error and never invents a tag.
    #[tokio::test]
    #[ignore = "downloads a model + runs CPU inference; opt-in via --ignored"]
    async fn embedded_tagger_loads_and_infers() {
        let preset = offline_model("qwen2.5-0.5b").expect("preset exists");
        let tagger = EmbeddedTagger::new(preset);
        let item = TaggerInput {
            id: 1,
            title: "Login page returns HTTP 500 on submit".to_string(),
            work_item_type: "Bug".to_string(),
            current_tags: vec![],
            description: None,
        };
        let allowed = vec![
            "type:bug".to_string(),
            "type:feature".to_string(),
            "area:auth".to_string(),
            "priority:high".to_string(),
        ];
        let out = tagger
            .suggest(&item, &allowed, &[], &Default::default())
            .await
            .expect("suggest should load the model and run inference");
        for s in &out {
            assert!(
                allowed.iter().any(|a| a.eq_ignore_ascii_case(&s.tag)),
                "suggested tag {:?} is not in the allowed set",
                s.tag
            );
        }
    }

    // Real, out-of-app latency benchmark for the CPU embedded path. Prints cold load
    // (incl. first download), warm load (parse from cache = ~= download-excluded), and
    // average per-query inference. MUST run in release (`--release`) - debug candle is
    // an order of magnitude slower and would mislead:
    //   cargo test -p poseiden-ai --release embedded_cpu_latency_bench -- --ignored --nocapture
    #[test]
    #[ignore = "downloads a model + runs CPU inference; opt-in benchmark via --ignored --nocapture"]
    fn embedded_cpu_latency_bench() {
        use std::time::Instant;
        let preset = offline_model("qwen2.5-0.5b").expect("preset exists");
        let allowed = [
            "type:bug",
            "area:frontend",
            "priority:high",
            "platform:mobile",
            "needs:triage",
        ];
        let item = TaggerInput {
            id: 0,
            title: "Login button unresponsive on mobile Safari after the latest deploy".to_string(),
            work_item_type: "Bug".to_string(),
            current_tags: vec![],
            description: None,
        };
        let prompt = chat_prompt(&build_prompt(
            &item,
            &allowed.map(String::from),
            &[],
            &Default::default(),
        ));

        let t = Instant::now();
        let mut loaded = load(preset).expect("cold load");
        let cold = t.elapsed();
        let t = Instant::now();
        let _warm_loaded = load(preset).expect("warm load"); // GGUF now cached -> parse only
        let warm_load = t.elapsed();

        let _ = generate(&mut loaded, &prompt).expect("warm-up gen"); // prime any lazy kernels
        let runs = 3u32;
        let t = Instant::now();
        let mut sample = String::new();
        for _ in 0..runs {
            sample = generate(&mut loaded, &prompt).expect("gen");
        }
        let per_query = t.elapsed() / runs;

        eprintln!(
            "EMBEDDED-CPU-BENCH model={} cold_load_incl_download={:.1}s warm_load_parse_only={:.1}s \
             infer_per_query_avg={:.2}s (download~={:.1}s) reply={:?}",
            preset.id,
            cold.as_secs_f64(),
            warm_load.as_secs_f64(),
            per_query.as_secs_f64(),
            (cold.as_secs_f64() - warm_load.as_secs_f64()).max(0.0),
            sample.trim(),
        );
    }
}
