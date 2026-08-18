# Project status

Where POSEIDON is **today**. For what's next see [ROADMAP.md](ROADMAP.md); for
everything else considered, [BACKLOG.md](BACKLOG.md).

## Working now

### Core pipeline
- **Provider polling (Azure DevOps, GitHub, GitLab)** - each provider normalises
  into the same provider-agnostic core shapes (WorkItem / Pipeline /
  PullRequest): Azure DevOps work items (WIQL query + batched detail fetch),
  pipelines (build definitions + windowed runs) and pull requests; GitHub Issues,
  Actions runs and PRs; GitLab Issues, Pipelines and MRs - active +
  recently-closed throughout. Public GitHub / GitLab repos poll without a token
  (a token is optional, for private repos or higher rate limits).
  (`poseidon-providers`)
- **Write-back (Azure DevOps)** - edit a work item's State + Tags
  (`Service::update_work_item`) and add/remove work-item↔PR links
  (`Service::link_work_item_pr`), written through the provider; hygiene flags
  recompute on the post-write view. Write-back is Azure DevOps only today; the
  GitHub and GitLab providers are read-focused. (`poseidon-server`)
- **Config-driven hygiene rules** - required tags, allowed/denied tag lists (with
  `type:*` wildcard patterns), untagged detection, per-state staleness limits,
  pipeline + pull-request checks, plus healthchecks: placeholder/bad-title and
  duplicate-title flags, a near-duplicate (TF-IDF) title scan, and orphaned-children
  detection (an open child under a Closed/Resolved parent). Pure engine, fully
  unit-tested. (`poseidon-rules`)
- **SQLite persistence** - `sqlx` with migrations embedded in the binary
  (auto-provisions on first run), upserts, list queries, and date-range work-item +
  pipeline report aggregates. (`poseidon-store`)
- **Poll scheduler** - polls once on startup, then on the configured interval.
  Per-project failures are logged and skipped, never fatal. (`poseidon-server`)

### Surfaces
- **Web / Docker instance** - axum API + static frontend, multi-stage Docker
  image, Helm chart with an RWO PVC template + Istio notes.
- **Desktop app** - Tauri shell embedding the same `Service`, reached via invoke.
  Builds to a native binary (~30 MB) on Windows today; Linux/macOS via CI.
- **CLI** - `poseidon poll` / `lint` / `report` / `config` / `tag`. `lint` exits
  non-zero on error-severity flags for CI gating; `tag` generates advisory AI tag
  suggestions.
- **Frontend** - Dashboard (with a Health-check flag breakdown), Work Items
  (per-column sort/filter, inline State/Tags/PR-link editing, flags joined per
  item, **and a Kanban board view** grouping the same items by State or any tag
  axis), Pull Requests (active + recently-closed, work-item link chips),
  Pipelines (status + last failure + log links), Reports (date range, closed-by-
  tag bar chart, success-rate gauge), and Rules (per-team hygiene policy, with
  override/inherited badge). Global config (repoint URL, the env-sourced instance
  settings, and a **Service-catalog CSV import**) lives in a Settings modal. Plain
  HTML/JS, dependency-free SVG charts, no build step.

### AI-assisted hygiene
- **Tag suggestions** - a deterministic keyword/alias engine (no model needed) plus
  an optional AI tagger; suggestions are advisory chips a person applies (POSEIDON
  never auto-applies). AI runs on an in-browser WebGPU model, an on-device embedded
  model, or a hosted provider, chosen per client.
- **Work-item field editor** - an in-app modal edits provider fields (dynamic
  type introspection, html↔markdown) with per-field **AI draft/improve** and an
  **"Improve all fields"** sweep: draft each field, harmonise them for consistency,
  then suggest tags - each result reviewed before it's applied.
- **AI activity queue** - one client-side queue runs a single heavy AI job at a
  time (a WebGPU GPU can't run two at once); the rest queue rather than block. A
  bottom activity bar shows progress, the queue, and a persistent completed list.
- **On-demand AI audit** - a healthcheck pass over selected items surfaces
  data-quality problems (vague titles, contradictory/boilerplate bodies) as
  advisory `ai_audit` flags.
- **Service-catalog integration** - a provider-agnostic `CatalogSource` (CSV export
  today; Port / Backstage stubbed) syncs a repo→product→team map, so `product:*`
  tags resolve from an item's linked repos instead of a hand-maintained list. See
  [design/catalog-integration.md](design/catalog-integration.md).

### Cross-cutting
- **Standalone-or-repointed clients** - a desktop/mobile client runs its own
  embedded instance, or is repointed at a hosted instance via one Settings field.
- **Portable mode** - all writes confined under `./.portable/` when enabled.
- **CI** - fmt/clippy/check, cargo-audit/deny/machete, a 3-OS test matrix, and
  Helm lint/template. Docker image → GHCR. Release workflow for desktop + CLI +
  Android.

## Validated

- **Unit tests** across core, paths, rules, reports, providers, store, doctor,
  ai, and server - including rule outcomes, report aggregation, provider
  normalisation from sample Azure DevOps, GitHub, and GitLab payloads, and
  path-resolution precedence.
- **End-to-end smoke** - the web server boots, auto-provisions the schema in
  portable mode, serves the API, and the frontend renders every view against the
  live API with no console errors. The CLI polls + lints against the same store.

## Known gaps / caveats

- **Demo mode is static-host only** - on a static host (GitHub Pages) the landing
  page offers a "View Demo" button that runs the app client-side against baked
  sample fixtures, no backend needed. The built-in `stub` provider (imported via
  the demo tenant bundle, `tenants/demo-data.poseidon.import.yaml`) gives the same
  deterministic dataset for the e2e + documentation screenshots. A native
  one-click `--demo` in the desktop shell is still a backlog item.
- **Icons are placeholders** - the Tauri bundle reuses a placeholder icon set;
  a POSEIDON-specific icon is a polish item.
- **iOS build not wired** - the release matrix covers desktop + Android; iOS is a
  backlog item (the primary mobile story is a repointed client).
