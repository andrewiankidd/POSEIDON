# Platform compatibility

POSEIDEN is **one codebase, one logic layer (`Service`)**, exposed as several
shells. Most features come from that shared core, so they light up everywhere the
core runs; the differences are about **where compute + data live** on each
platform. This table maps features to platforms so you know what to expect.

## Shells

- **Desktop** - native app (Tauri) embedding the full `Service` over a local
  SQLite store. Standalone, or repointed at a remote instance.
- **Android** - the same app on mobile (Tauri). Same core; tighter CPU/RAM.
- **Hosted (web)** - the `Service` in a container (Docker / Helm), used from a
  browser. Multi-user when fronted by the auth proxy.
- **CLI** - `poseiden poll` / `lint` / `report` over the same `Service`.
- **Client mode** - a browser on a **static host** (GitHub Pages) or a repointed
  desktop/mobile app. It holds **no data or compute of its own** - it shows
  whatever the **server it points at** supports (see the last row).

## Legend

✅ supported · ➖ not applicable · ⏳ planned (not built) · ⚠ caveat (see notes)

| Capability | Desktop | Android | Hosted (web) | CLI |
|---|:---:|:---:|:---:|:---:|
| Poll Azure DevOps / GitHub / GitLab + all views (dashboard / work items / PRs / pipelines / reports) | ✅ | ✅ | ✅ | ⚠ [1] |
| Hygiene rules + flags | ✅ | ✅ | ✅ | ✅ |
| Inline write-back (State / Tags, WI↔PR links) - Azure DevOps only [10] | ✅ | ✅ | ✅ | ➖ |
| Configurable report engine | ✅ | ✅ | ✅ | ⚠ [2] |
| Standalone (local data on the device) | ✅ | ✅ | ➖ [3] | ✅ |
| Repoint at a remote instance (client) | ✅ | ✅ | ➖ [3] | ➖ |
| Multi-user auth · per-user tenants | ➖ [4] | ➖ [4] | ✅ | ➖ [4] |
| Azure DevOps auth: PAT | ✅ | ✅ | ✅ | ✅ |
| Azure DevOps auth: native device-code sign-in (no `az` CLI) | ✅ | ✅ | ✅ | ⚠ [5] |
| GitHub / GitLab auth: optional token env var (anonymous for public repos) | ✅ | ✅ | ✅ | ✅ |
| AI tags: online / self-hosted (Claude / Gemini / GPT, or any OpenAI-compatible endpoint - your own Ollama / LM Studio / vLLM) | ✅ | ✅ | ✅ | ➖ |
| AI tags: offline embedded model (no Ollama), CPU | ✅ ⚠ [6] | ⚠ [7] | ✅ ⚠ [6] | ➖ |
| AI tags: offline embedded model on GPU (CUDA build) | ⚠ [9] | ➖ | ⚠ [9] | ➖ |
| AI config: per-user (each owner their own provider/model/key) | ➖ [4] | ➖ [4] | ✅ | ➖ [4] |
| Bulk retag (multi-select apply add/remove tag, set state) | ✅ | ✅ | ✅ | ➖ |
| Serverless in the browser (no install, no host) | ➖ | ➖ | ➖ | ➖ [8] |
| Client shows the server's features (data, AI, …) | ✅ | ✅ | ✅ | ➖ |

## Notes

1. **CLI** exposes `poll` / `lint` / `report` (no interactive views); the data
   itself is the same as the app.
2. **CLI report parity** - `poseiden report` still emits the fixed flow summary,
   not the configurable report engine the GUI uses (tracked in `BACKLOG.md`).
3. A hosted instance *is* the server, so "standalone" / "repoint" don't apply to
   it; its browser UI is same-origin to the server.
4. Standalone / CLI run as the single `default` owner - one tenant, so per-user
   auth and per-user AI config don't apply. Multi-user + per-owner is the hosted
   (auth-on) mode.
5. Interactive device-code sign-in is a GUI action; the CLI runs with a PAT, or
   reuses a session already signed in via the app (shared per-owner token cache).
6. **Offline embedded model loads + runs end-to-end** (validated by dogfooding:
   first use downloads the model to a writable cache, loads it, and infers). The
   catch is **speed**: in-process CPU inference in a hosted **container** is slow
   (minutes per item), so the run is a **background job** the UI polls, not a
   blocking request. A **desktop** (real CPU, one item interactively) is the
   practical home for offline; for **hosted**, an online provider is the comfortable
   choice. Loaded models are shared process-wide by id. Output *quality* of the
   tiny 0.5-1.5B models still wants tuning.
7. **Android** can run the embedded model, but only a small one, slower and
   RAM-limited; a hosted or online backend is the more comfortable mobile choice.
8. A browser can't run the `Service` (no local Rust/SQLite) and Azure DevOps
   doesn't allow direct browser (CORS) calls, so a **truly serverless, real-data**
   experience isn't possible - the browser is always a client of a Service. A
   demo-data-only in-browser playground is a separate, possible idea (see
   `BACKLOG.md`).
9. **GPU (CUDA) embedded model** - the code is GPU-ready (`Device::cuda_if_available`
   behind a `cuda` cargo feature, threaded `poseiden-ai` -> `poseiden-server` ->
   `poseiden-app`), but off by default: the normal build stays pure-CPU and runs
   anywhere. Build a GPU desktop binary on a machine with the CUDA toolkit:
   `cargo tauri build --features cuda --config crates/poseiden-app/tauri.conf.json`.
   It links CUDA and needs an NVIDIA GPU + runtime to load, so it ships as a
   **separate** artifact, not the default download. Caveat: candle's *quantized*
   (GGUF) CUDA path is less mature than CPU - an f16 model may be needed. Not yet
   run end-to-end on real hardware. (Container/hosted has no GPU, so use an online
   or local-endpoint provider there.)
10. **Write-back is Azure DevOps only.** Polling, hygiene, reports, and every view
    work across Azure DevOps, GitHub, and GitLab. Inline State/Tags edits and
    work-item↔PR links write back through the Azure DevOps provider; the GitHub
    and GitLab providers are read-focused today.

See also: [PROJECT_STATUS.md](PROJECT_STATUS.md) (what works today),
[user-guide.md](features/user-guide.md) (shells + storage), and
[SCOPE.md](SCOPE.md) (deliberate non-goals).
