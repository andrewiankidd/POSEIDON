// Centralized CLIENT-side AI dispatch - the single place that decides WHERE an AI
// query runs for this client and executes it. Two loci exist by necessity: the browser
// (WebGPU, on the user's own GPU via web-llm - the server can't touch it) and the
// server (online / embedded, behind the `AiTagger` trait). Every AI feature routes its
// backend choice through here instead of re-deciding inline, so "webgpu vs server"
// lives in exactly one spot on the client - the mirror of the server's `AiTagger`.
//
// The server-side counterpart uses a value-OR-prompt handshake: an endpoint runs the
// query itself when it has a server model, else returns the built prompt for the
// browser to run. `resolveAiText` closes that loop.

import { api } from './api.js';
import { webgpuAvailable, runWebGpuChat } from './webgpu.js';

// What actually runs for THIS client, in priority order: WebGPU entries are usable only
// where the browser has WebGPU; every other kind uses the server's platform verdict.
// First usable + configured one wins (matches the Suggest engine choice).
export function effectiveActiveId(list, caps) {
  const usable = (i) => (i.kind === 'webgpu' ? webgpuAvailable() : i.compatible) && i.configured !== false;
  const hit = (list || []).find(usable);
  return hit ? hit.id : null;
}

// The active AI backend for this client:
//   { where: 'browser', model, id }  -> WebGPU, runs in-page on the user's GPU
//   { where: 'server', kind, id }    -> online / embedded, runs behind the server API
//   { where: 'none' }                -> nothing configured/usable
export async function activeBackend() {
  let cfg;
  try { cfg = await api.llmConfig(); } catch { return { where: 'none' }; }
  const activeId = effectiveActiveId(cfg.integrations || [], cfg.caps || {});
  const top = (cfg.integrations || []).find((i) => i.id === activeId);
  if (!top) return { where: 'none' };
  if (top.kind === 'webgpu') return { where: 'browser', model: top.offline_model || null, id: top.id };
  return { where: 'server', kind: top.kind, id: top.id };
}

// Resolve a server AI response of shape `{ value }` | `{ prompt: { system, user } }`
// to final text. If the server produced text (it had a server model), return it. If it
// handed back a prompt (its only model runs in the browser), run that prompt on the
// active WebGPU model. The one place the "server-first, browser-fallback" single-
// completion pattern lives - any feature whose endpoint returns value-or-prompt uses it.
// `onStatus(text)` surfaces model-load progress during a browser run.
export async function resolveAiText(res, onStatus) {
  let text;
  if (res && typeof res.value === 'string') {
    text = res.value;
  } else if (res && res.prompt) {
    const be = await activeBackend();
    if (be.where !== 'browser' || !be.model) {
      throw new Error('No AI model configured (Settings → AI)');
    }
    text = await runWebGpuChat(be.model, res.prompt.system, res.prompt.user, onStatus);
  } else {
    throw new Error('AI returned no result');
  }
  return fixMarkdownLinks(text);
}

// Deterministic backstop for the "never lose information" rule. A small local model
// will still occasionally drop an inline image/attachment (or a link) when rewriting a
// field, whatever the prompt says. Re-append any markdown image `![alt](url)` or link
// `[text](url)` whose URL was in the ORIGINAL but is missing from the REWRITE, so an
// attachment can never be silently lost - placement may shift to the end, but the
// information survives. De-dupes by URL; a draft from empty original is a no-op.
export function preserveMarkdownAssets(original, rewritten) {
  if (!original || rewritten == null) return rewritten;
  const seen = new Set();
  const missing = [];
  for (const m of original.matchAll(/!?\[[^\]]*\]\(([^)]+)\)/g)) {
    const token = m[0];
    const url = m[1].trim();
    if (!url || seen.has(url)) continue;
    seen.add(url);
    if (!rewritten.includes(url)) missing.push(token);
  }
  if (!missing.length) return rewritten;
  return rewritten.replace(/\s+$/, '') + '\n\n' + missing.join('\n');
}

// Small models sometimes echo a field's own name into its value ("Description: …" /
// "**Acceptance Criteria:**\n…"). Strip a single leading "<Label>:" (optionally bold,
// any case) so the label never leaks into the saved content.
export function stripFieldLabelPrefix(value, label) {
  if (!value || !label) return value;
  const esc = label.trim().replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return value.replace(new RegExp('^\\s*\\**\\s*' + esc + '\\s*\\**\\s*:\\s*\\n?', 'i'), '');
}

// Repair the common small-model link mistake: a bare URL inside [brackets] with no
// (target) - e.g. "[Search URL: https://…]" or "[https://…]" - which is NOT a markdown
// link and renders as literal text (and saves back broken). Rewrite to a valid
// [label](url). Only touches brackets that CONTAIN a URL and aren't already a proper
// [text](url) link, so "[TODO: …]" placeholders and real links are left untouched.
export function fixMarkdownLinks(md) {
  if (!md) return md;
  return md.replace(/\[([^\]]*?(https?:\/\/[^\]\s]+)[^\]]*?)\](?!\()/g, (_m, inner, url) => {
    let label = inner.replace(url, '').replace(/[\s:–—-]+$/, '').replace(/^[\s:–—-]+/, '').trim();
    if (!label) label = url;
    return `[${label}](${url})`;
  });
}
