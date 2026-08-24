// In-browser LLM inference on WebGPU (EXPERIMENTAL). The model runs entirely in the
// user's browser on their GPU via web-llm (MLC), loaded lazily from a CDN - no build
// step, no bundler. Results are posted to the server, which RE-VALIDATES them against
// the allowed tags (the trust boundary), so nothing here can inject arbitrary tags.
//
// NOT verified end-to-end: needs a WebGPU browser (Chrome/Edge) + a real GPU to run.
// The prompt + validation logic below MIRRORS the Rust `poseidon-ai` versions by hand
// (the browser engine can't call the crate); keep them in sync.

const WEBLLM_CDN = 'https://esm.run/@mlc-ai/web-llm';

// Preset id -> web-llm model id, and the largest->smallest fallback ladder. BOTH are
// derived at runtime from the server's model catalog (poseidon-ai's OFFLINE_MODELS, which
// carries each model's `webgpu_id` + `min_vram_mb`) via registerModels() - so swapping the
// model family is a one-place edit in presets.rs, with NO ids hardcoded here.
let MODEL_MAP = {};
let MODEL_LADDER = [];

/** Register the model catalog (the server's `presets.offline`) so this module can resolve
 *  web-llm ids and the load-fallback ladder without hardcoding them. Call once the
 *  /llm-config presets are fetched. Ignores presets without a `webgpu_id`. */
export function registerModels(presets) {
  const list = (presets || []).filter((p) => p && p.id && p.webgpu_id);
  MODEL_MAP = Object.fromEntries(list.map((p) => [p.id, p.webgpu_id]));
  MODEL_LADDER = list
    .slice()
    .sort((a, b) => (b.min_vram_mb || 0) - (a.min_vram_mb || 0))
    .map((p) => p.id);
}

// Mirror of poseidon-ai's SUGGESTION_SLACK + default_max_suggestions: the AI's
// per-item tag ceiling scales with the required-tag axes (so product/area/source all
// fit, plus room for a second area / a rewrite), floored so a no-required team still
// gets a usable ceiling. A team can override via rules.max_suggestions.
const SUGGESTION_SLACK = 7;
function defaultMaxSuggestions(requiredLen) {
  return Math.max(6, (requiredLen || 0) + SUGGESTION_SLACK);
}

// Mirror of poseidon-ai's SYSTEM_PROMPT.
const SYSTEM_PROMPT =
  'You tag software work items for a backlog-hygiene tool. You are given one work ' +
  'item, a list of ALLOWED tags, and possibly some REQUIRED categories. For each ' +
  'REQUIRED category you MUST choose exactly ONE value from the options listed for it ' +
  '- make your BEST GUESS from the title/type/description even when you are not fully ' +
  'certain; a reasonable guess is better than leaving a required category empty, so ' +
  'never skip one. List the required picks FIRST. For all OTHER (optional) ALLOWED ' +
  'tags, favour precision: add one only when it CLEARLY applies, and prefer returning ' +
  'fewer (or none) over a long list. Never pick tags that contradict each other. ' +
  'Some tags show example keywords in parentheses - e.g. `area:foo (e.g. bar, baz)` - ' +
  'which clarify what that tag MEANS; use them to pick the right tag, but output ONLY ' +
  'the tag itself (`area:foo`), never the examples. ' +
  'Never invent tags or use any tag not in the ALLOWED list. Reply with ONLY a JSON ' +
  'object, no prose, no code fences: {"tags": ["<allowed tag>", ...], "rationale": ' +
  '"<one short sentence>"}. If there are no required categories and nothing else ' +
  'clearly applies, reply {"tags": [], "rationale": "none apply"}.';

// Mirror of poseidon-ai's pattern_matches: trailing-`*` prefix wildcard, else
// exact, case-insensitive.
function patternMatches(pattern, tag) {
  const p = (pattern || '').trim().toLowerCase();
  const t = (tag || '').trim().toLowerCase();
  return p.endsWith('*') ? t.startsWith(p.slice(0, -1)) : p === t;
}

// Mirror of poseidon-ai's MAX_HINTS_PER_TAG + annotate: show a tag with its keyword
// gloss as `tag (e.g. kw1, kw2)` so the model knows what the tag denotes.
const MAX_HINTS_PER_TAG = 6;
function annotate(tag, hints) {
  const kw = (hints && hints[tag.toLowerCase()]) || [];
  const shown = kw.map((k) => (k || '').trim()).filter(Boolean).slice(0, MAX_HINTS_PER_TAG);
  return shown.length ? `${tag} (e.g. ${shown.join(', ')})` : tag;
}

/** True when this browser exposes WebGPU (Chrome/Edge; behind a flag elsewhere). */
export function webgpuAvailable() {
  return typeof navigator !== 'undefined' && !!navigator.gpu;
}


/**
 * Browser-only platform capabilities the server can't see, POSTed to the autotune
 * endpoint so it can size the WebGPU/CPU models. We deliberately do NOT report a
 * VRAM number: WebGPU exposes no reliable total-VRAM figure (adapter limits are
 * per-buffer caps, not device memory), and a misleading low number would pick a
 * worse model than the safe default. So the server sees webgpu availability + a
 * coarse RAM/core count and defaults WebGPU to a strong-but-safe model; the
 * benchmark 'tune' + the load fallback ladder are the reliable path higher.
 */
// WebGPU exposes no VRAM figure, so MEASURE how much GPU memory we can actually
// allocate: grab STORAGE buffers (capped at the adapter's max buffer size) until an
// allocation trips the out-of-memory error scope, then free them all. The total we
// reached is a conservative floor on usable VRAM - enough to t-shirt-size the model
// WITHOUT downloading it first (so a 750 Ti never pulls a 7B). Bounded (32 GB ceiling),
// no writes, no model load; best-effort - any failure leaves vram unmeasured and the
// caller falls back to the conservative default (then the load ladder still protects us).
async function probeVramMb(adapter) {
  let device;
  try { device = await adapter.requestDevice(); } catch { return null; }
  const CHUNK = Math.min(adapter.limits?.maxBufferSize || (256 * 1024 * 1024), 256 * 1024 * 1024);
  const CEILING = 32 * 1024 * 1024 * 1024; // don't probe past 32 GB
  const buffers = [];
  let bytes = 0;
  try {
    while (bytes + CHUNK <= CEILING) {
      device.pushErrorScope('out-of-memory');
      const buf = device.createBuffer({ size: CHUNK, usage: GPUBufferUsage.STORAGE });
      const err = await device.popErrorScope();
      if (err) { try { buf.destroy(); } catch { /* ignore */ } break; }
      buffers.push(buf);
      bytes += CHUNK;
    }
  } catch { /* stop on any unexpected error - keep what we measured */ }
  for (const b of buffers) { try { b.destroy(); } catch { /* ignore */ } }
  try { device.destroy(); } catch { /* ignore */ }
  return bytes > 0 ? Math.round(bytes / (1024 * 1024)) : null;
}

export async function detectBrowserCaps() {
  const nav = typeof navigator !== 'undefined' ? navigator : {};
  let webgpu = false;
  let adapter = null;
  if (nav.gpu) {
    try {
      adapter = await nav.gpu.requestAdapter();
      // A software/fallback adapter isn't worth running a real model on.
      webgpu = !!adapter && !adapter.isFallbackAdapter;
    } catch { webgpu = false; adapter = null; }
  }
  const caps = { embedded: false, gpu: false, webgpu };
  if (webgpu && adapter) {
    // Measure usable VRAM so the model auto-sizes to this GPU (no manual picking, no
    // speculative big download). Leaves vram_mb unset if the probe can't run.
    try {
      const vram = await probeVramMb(adapter);
      if (vram) caps.vram_mb = vram;
    } catch { /* best-effort */ }
  }
  if (typeof nav.deviceMemory === 'number' && nav.deviceMemory > 0) {
    caps.ram_mb = Math.round(nav.deviceMemory * 1024); // coarse (capped at 8 by the browser)
  }
  if (typeof nav.hardwareConcurrency === 'number' && nav.hardwareConcurrency > 0) {
    caps.cpu_cores = nav.hardwareConcurrency;
  }
  return caps;
}

// Mirror of poseidon-ai's MAX_DESC_CHARS - cap the body so the prompt stays bounded.
const MAX_DESC_CHARS = 1000;

// Mirror of poseidon-ai's build_prompt: unsatisfied required patterns become
// must-fill categories (each with its concrete options); everything else in
// `allowed` is offered as optional.
function buildUserPrompt(item, allowed, required = [], hints = {}, background = '') {
  const current = (item.tags || []);
  const cur = current.length ? current.join(', ') : '(none)';
  const desc = (item.description || '').trim();
  const descLine = desc ? `\n- Description: ${desc.slice(0, MAX_DESC_CHARS)}` : '';

  const requiredBlocks = [];
  const claimed = new Set();
  for (const pat of required) {
    const p = (pat || '').trim();
    if (!p) continue;
    if (current.some((t) => patternMatches(p, t))) continue; // already satisfied
    const options = allowed.filter((a) => patternMatches(p, a));
    if (!options.length) continue;
    for (const o of options) claimed.add(o.toLowerCase());
    const label = p.endsWith('*') ? p.slice(0, -1) : p;
    requiredBlocks.push(`- ${label} (choose one): ${options.map((o) => annotate(o, hints)).join(', ')}`);
  }
  const optional = allowed.filter((a) => !claimed.has(a.toLowerCase()));

  let out = '';
  const bg = (background || '').trim();
  if (bg) {
    out += "TEAM BACKGROUND (this team's systems + internal names - use it to interpret the item):\n" + bg + '\n\n';
  }
  out += `Work item:\n- Title: ${item.title || ''}\n- Type: ${item.work_item_type || ''}${descLine}` +
    `\n- Current tags: ${cur}\n`;
  if (requiredBlocks.length) {
    out += '\nREQUIRED categories - pick exactly ONE value for EACH (best guess even if unsure):\n' +
      requiredBlocks.join('\n') + '\n';
  }
  if (optional.length) {
    out += (requiredBlocks.length ? '\nOPTIONAL tags (only if they CLEARLY apply):\n' : '\nALLOWED tags:\n') +
      optional.map((t) => `- ${annotate(t, hints)}`).join('\n') + '\n';
  }
  out += '\nReturn the JSON now.';
  return out;
}

function extractJson(s) {
  const t = (s || '').trim();
  const a = t.indexOf('{');
  const b = t.lastIndexOf('}');
  return a >= 0 && b > a ? t.slice(a, b + 1) : '{}';
}

// Mirror of poseidon-ai's parse_audit_response (same JSON shape: {"issues":[{kind,
// detail}]}) so the browser can show live per-item audit detail. Best-effort: returns
// null when the text isn't parseable (so the caller can say "audited" rather than
// falsely "clean"); the server re-parses authoritatively for the stored flags.
export function parseAudit(text) {
  let reply;
  try { reply = JSON.parse(extractJson(stripThink(text))); } catch { return null; }
  const issues = Array.isArray(reply && reply.issues) ? reply.issues : [];
  return issues
    .filter((i) => i && i.kind && String(i.detail || '').trim())
    .map((i) => ({ kind: String(i.kind), detail: String(i.detail).trim() }));
}

// Mirror of poseidon-ai's parse_suggestions: keep only allowed tags (canonical
// spelling), dedupe, cap at `max`. The server re-validates too.
function parseSuggestions(text, allowed, max) {
  let reply = {};
  try { reply = JSON.parse(extractJson(text)); } catch { reply = {}; }
  const canon = new Map(allowed.map((t) => [t.toLowerCase(), t]));
  const rationale = String(reply.rationale || '').trim() || 'AI suggestion';
  const seen = new Set();
  const out = [];
  for (const raw of reply.tags || []) {
    if (out.length >= max) break;
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

/** Resolve our preset id to a web-llm model id (exported for the download UI). Falls back
 *  to the smallest registered model, then the raw id, if the map isn't populated yet. */
export function webGpuModelId(offlineModel) {
  return MODEL_MAP[offlineModel]
    || MODEL_MAP[MODEL_LADDER[MODEL_LADDER.length - 1]]
    || offlineModel;
}

// Qwen3 has a "thinking" mode that emits a chain-of-thought before answering - pure wasted
// tokens (and latency) for our structured tag/draft output. The `/no_think` soft-switch in
// the system message disables it; models that don't recognise it (Qwen2.5, Llama) ignore it.
const NO_THINK = '\n\n/no_think';

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
 * Load the requested model, stepping DOWN the ladder on a load failure (typically
 * out-of-VRAM). This is what makes optimistic model selection safe: a machine that
 * can't fit the chosen model auto-settles on the largest one it can. Returns the
 * engine; throws only if even the smallest model won't load.
 */
async function loadWithFallback(offlineModel, onStatus) {
  // Ladder not populated yet (registerModels hasn't run, or a run fired before boot
  // finished) - load the one requested model directly rather than throwing.
  if (!MODEL_LADDER.length) {
    return getEngine(webGpuModelId(offlineModel), (p) => onStatus && onStatus(p && p.text ? p.text : 'Loading model…'));
  }
  // indexOf === -1 (requested model not in the ladder) → start at the top and step down.
  const idx = MODEL_LADDER.indexOf(offlineModel);
  const start = idx < 0 ? 0 : idx;
  let lastErr;
  for (let i = start; i < MODEL_LADDER.length; i++) {
    const id = MODEL_LADDER[i];
    try {
      const stepping = i > start;
      return await getEngine(MODEL_MAP[id], (p) => {
        const base = p && p.text ? p.text : 'Loading model…';
        onStatus && onStatus(stepping ? `${id} (fallback): ${base}` : base);
      });
    } catch (e) {
      lastErr = e;
      // Reset the cached engine promise so the next-smaller model can load fresh.
      enginePromise = null; engineModel = null;
      onStatus && onStatus(`${id} didn't load, trying a smaller model…`);
    }
  }
  throw lastErr || new Error('No WebGPU model could be loaded.');
}

/**
 * Run tag inference over `items` IN THE BROWSER on the GPU. Returns
 * `[{ id, tags: [{ tag, reason }] }]` (already validated against `allowed`).
 * `required` is the team's required-tag patterns (e.g. `["area:*","source:*"]`) so
 * the model best-effort fills a must-fill slot the item doesn't yet satisfy.
 * `onStatus(text)` reports model-load progress; `onItem(done,total,count)` per item.
 */
export async function runWebGpuTagging(offlineModel, items, allowed, onStatus, onItem, required = [], hints = {}, background = '', maxSuggestions = null) {
  if (!webgpuAvailable()) throw new Error('WebGPU is not available in this browser.');
  if (!allowed || !allowed.length) return [];
  // Configured ceiling, else the adaptive default that scales with required axes.
  const cap = Number.isInteger(maxSuggestions) && maxSuggestions > 0
    ? maxSuggestions
    : defaultMaxSuggestions(required.length);
  const engine = await loadWithFallback(offlineModel, onStatus);
  const results = [];
  for (let i = 0; i < items.length; i++) {
    if (onItem) onItem(i, items.length, null);
    const resp = await engine.chat.completions.create({
      messages: [
        { role: 'system', content: SYSTEM_PROMPT + NO_THINK },
        { role: 'user', content: buildUserPrompt(items[i], allowed, required, hints, background) },
      ],
      temperature: 0.2,
      max_tokens: 200,
    });
    const text =
      (resp && resp.choices && resp.choices[0] && resp.choices[0].message &&
        resp.choices[0].message.content) || '';
    const tags = parseSuggestions(stripThink(text), allowed, cap);
    // Diagnostic: WebGPU inference is client-only, so the server can't log why an item
    // got zero tags. Surface the raw model output + what survived allow-list filtering
    // here, so an empty Suggested cell is explainable from DevTools (not a black box).
    if (!tags.length) {
      console.warn(`[tagger] #${items[i].id} "${(items[i].title || '').slice(0, 60)}" -> 0 tags.`,
        { rawModelOutput: text, allowedCount: allowed.length, required });
    } else {
      console.debug(`[tagger] #${items[i].id} -> ${tags.length} tags: ${tags.join(', ')}`, { rawModelOutput: text });
    }
    results.push({ id: items[i].id, tags });
    // Third arg carries the finished item's result (id + the tags that survived
    // allow-list filtering) so the caller can show a live per-item row; the first
    // two args stay the done/total progress count.
    if (onItem) onItem(i + 1, items.length, { id: items[i].id, tags });
  }
  return results;
}

/** Run one free-form chat completion through the browser WebGPU model - the generic
 *  in-browser inference primitive (used by the field editor's AI Draft/Improve, and any
 *  other single-completion feature). The server builds the system+user prompt (so prompt
 *  logic lives in one place) and hands it here. Returns the generated text (an outer
 *  ```fence stripped). `onStatus` reports model-load progress. */
export async function runWebGpuChat(offlineModel, system, user, onStatus) {
  if (!webgpuAvailable()) throw new Error('WebGPU is not available in this browser.');
  const engine = await loadWithFallback(offlineModel, onStatus);
  const resp = await engine.chat.completions.create({
    messages: [
      { role: 'system', content: (system || '') + NO_THINK },
      { role: 'user', content: user },
    ],
    temperature: 0.3,
    max_tokens: 700,
  });
  const text =
    (resp && resp.choices && resp.choices[0] && resp.choices[0].message &&
      resp.choices[0].message.content) || '';
  return stripOuterFence(stripThink(text));
}

// Qwen3 emits a <think>…</think> block even with /no_think - usually empty, but always
// present - and it leaks into drafts / parsed output. Strip the whole block (and any stray
// unmatched tag) wherever we consume raw model text.
function stripThink(s) {
  return String(s || '')
    .replace(/<think>[\s\S]*?<\/think>/gi, '')
    .replace(/<\/?think>/gi, '')
    .trim();
}

// Drop a single outer ```lang … ``` fence if the model wrapped its whole answer in one
// (mirrors poseidon-ai's strip_code_fence).
function stripOuterFence(s) {
  const t = s.trim();
  if (t.startsWith('```')) {
    const nl = t.indexOf('\n');
    if (nl > 0) {
      const inner = t.slice(nl + 1);
      const close = inner.lastIndexOf('```');
      if (close >= 0) return inner.slice(0, close).trim();
    }
  }
  return t;
}
