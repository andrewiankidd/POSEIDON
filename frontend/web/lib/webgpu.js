// In-browser LLM inference on WebGPU (EXPERIMENTAL). The model runs entirely in the
// user's browser on their GPU via web-llm (MLC), loaded lazily from a CDN - no build
// step, no bundler. Results are posted to the server, which RE-VALIDATES them against
// the allowed tags (the trust boundary), so nothing here can inject arbitrary tags.
//
// NOT verified end-to-end: needs a WebGPU browser (Chrome/Edge) + a real GPU to run.
// The prompt + validation logic below MIRRORS the Rust `poseiden-ai` versions by hand
// (the browser engine can't call the crate); keep them in sync.

const WEBLLM_CDN = 'https://esm.run/@mlc-ai/web-llm';

// Our offline preset ids -> web-llm prebuilt model ids.
const MODEL_MAP = {
  'qwen2.5-0.5b': 'Qwen2.5-0.5B-Instruct-q4f16_1-MLC',
  'qwen2.5-1.5b': 'Qwen2.5-1.5B-Instruct-q4f16_1-MLC',
  'qwen2.5-3b': 'Qwen2.5-3B-Instruct-q4f16_1-MLC',
  'qwen2.5-7b': 'Qwen2.5-7B-Instruct-q4f16_1-MLC',
};

const MAX_SUGGESTIONS = 3;

// Mirror of poseiden-ai's SYSTEM_PROMPT.
const SYSTEM_PROMPT =
  'You tag software work items for a backlog-hygiene tool. You are given one work ' +
  'item and a list of ALLOWED tags. Pick only the FEW most applicable tags - AT MOST ' +
  '3, and only ones you are confident CLEARLY apply. Favour precision over coverage: ' +
  'when in doubt, leave a tag out, and prefer returning fewer (or none) over a long ' +
  'list. Never pick tags that contradict each other. Never invent tags or use any tag ' +
  'not in the ALLOWED list. Reply with ONLY a JSON object, no prose, no code fences: ' +
  '{"tags": ["<allowed tag>", ...], "rationale": "<one short sentence>"}. If none ' +
  'clearly apply, reply {"tags": [], "rationale": "none apply"}.';

/** True when this browser exposes WebGPU (Chrome/Edge; behind a flag elsewhere). */
export function webgpuAvailable() {
  return typeof navigator !== 'undefined' && !!navigator.gpu;
}

// Mirror of poseiden-ai's MAX_DESC_CHARS - cap the body so the prompt stays bounded.
const MAX_DESC_CHARS = 1000;

function buildUserPrompt(item, allowed) {
  const cur = item.tags && item.tags.length ? item.tags.join(', ') : '(none)';
  const list = allowed.map((t) => `- ${t}`).join('\n');
  const desc = (item.description || '').trim();
  const descLine = desc ? `\n- Description: ${desc.slice(0, MAX_DESC_CHARS)}` : '';
  return `Work item:\n- Title: ${item.title || ''}\n- Type: ${item.work_item_type || ''}${descLine}` +
    `\n- Current tags: ${cur}\n\nALLOWED tags:\n${list}\n\nReturn the JSON now.`;
}

function extractJson(s) {
  const t = (s || '').trim();
  const a = t.indexOf('{');
  const b = t.lastIndexOf('}');
  return a >= 0 && b > a ? t.slice(a, b + 1) : '{}';
}

// Mirror of poseiden-ai's parse_suggestions: keep only allowed tags (canonical
// spelling), dedupe, cap at MAX_SUGGESTIONS. The server re-validates too.
function parseSuggestions(text, allowed) {
  let reply = {};
  try { reply = JSON.parse(extractJson(text)); } catch { reply = {}; }
  const canon = new Map(allowed.map((t) => [t.toLowerCase(), t]));
  const rationale = String(reply.rationale || '').trim() || 'AI suggestion';
  const seen = new Set();
  const out = [];
  for (const raw of reply.tags || []) {
    if (out.length >= MAX_SUGGESTIONS) break;
    const key = String(raw).trim().toLowerCase();
    const c = canon.get(key);
    if (c && !seen.has(key)) { seen.add(key); out.push({ tag: c, reason: rationale }); }
  }
  return out;
}

let enginePromise = null;
let engineModel = null;
// `onReport(report)` gets web-llm's progress object: { progress: 0..1, text, timeElapsed }.
async function getEngine(modelId, onReport) {
  if (enginePromise && engineModel === modelId) return enginePromise;
  engineModel = modelId;
  enginePromise = (async () => {
    const webllm = await import(/* webpackIgnore: true */ WEBLLM_CDN);
    return webllm.CreateMLCEngine(modelId, {
      initProgressCallback: (p) => onReport && onReport(p || {}),
    });
  })();
  return enginePromise;
}

/** Resolve our preset id to a web-llm model id (exported for the download UI). */
export function webGpuModelId(offlineModel) {
  return MODEL_MAP[offlineModel] || MODEL_MAP['qwen2.5-0.5b'];
}

/**
 * Whether this model is already fully cached in the browser (Cache Storage /
 * IndexedDB), so no download is needed. Lets the UI disable the Download button
 * instead of re-fetching. Best-effort: any error (offline, old web-llm) -> false.
 */
export async function isModelCached(offlineModel) {
  if (!webgpuAvailable()) return false;
  try {
    const webllm = await import(/* webpackIgnore: true */ WEBLLM_CDN);
    if (typeof webllm.hasModelInCache !== 'function') return false;
    return await webllm.hasModelInCache(webGpuModelId(offlineModel));
  } catch {
    return false;
  }
}

/**
 * Pre-download + compile a model into the browser cache, reporting fractional
 * progress via `onReport({ progress, text })`. Idempotent - once cached (Cache
 * Storage / IndexedDB) later loads are instant. Lets the model-management UI show a
 * progress bar instead of waiting until the first suggestion run.
 */
export async function prepareModel(offlineModel, onReport) {
  if (!webgpuAvailable()) throw new Error('WebGPU is not available in this browser.');
  await getEngine(webGpuModelId(offlineModel), onReport);
}

/**
 * Run tag inference over `items` IN THE BROWSER on the GPU. Returns
 * `[{ id, tags: [{ tag, reason }] }]` (already validated against `allowed`).
 * `onStatus(text)` reports model-load progress; `onItem(done,total,count)` per item.
 */
export async function runWebGpuTagging(offlineModel, items, allowed, onStatus, onItem) {
  if (!webgpuAvailable()) throw new Error('WebGPU is not available in this browser.');
  if (!allowed || !allowed.length) return [];
  const modelId = MODEL_MAP[offlineModel] || MODEL_MAP['qwen2.5-0.5b'];
  const engine = await getEngine(modelId, (p) => onStatus && onStatus(p && p.text ? p.text : 'Loading model…'));
  const results = [];
  for (let i = 0; i < items.length; i++) {
    if (onItem) onItem(i, items.length, 0);
    const resp = await engine.chat.completions.create({
      messages: [
        { role: 'system', content: SYSTEM_PROMPT },
        { role: 'user', content: buildUserPrompt(items[i], allowed) },
      ],
      temperature: 0.2,
      max_tokens: 200,
    });
    const text =
      (resp && resp.choices && resp.choices[0] && resp.choices[0].message &&
        resp.choices[0].message.content) || '';
    const tags = parseSuggestions(text, allowed);
    results.push({ id: items[i].id, tags });
    if (onItem) onItem(i + 1, items.length, tags.length);
  }
  return results;
}
