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
  if (res && typeof res.value === 'string') return res.value;
  if (res && res.prompt) {
    const be = await activeBackend();
    if (be.where !== 'browser' || !be.model) {
      throw new Error('No AI model configured (Settings → AI)');
    }
    return runWebGpuChat(be.model, res.prompt.system, res.prompt.user, onStatus);
  }
  throw new Error('AI returned no result');
}
