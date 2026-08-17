# Changelog

All notable changes to POSEIDON are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Service-catalog integration** — a provider-agnostic `CatalogSource` (CSV export
  today; Port / Backstage stubbed behind the same trait) syncs a repo→product→team
  map into a `catalog` table, so `product:*` tags resolve from an item's linked repos
  instead of a hand-maintained list. Upload a Port "Service" CSV via **Settings →
  Import / Export → Service catalog** (or `POST /api/catalog/import`); raw product ids
  canonicalise to taxonomy slugs via a `catalog.product_aliases` map. The static
  `repo_tags` rules become the manual override layer. See
  [docs/design/catalog-integration.md](docs/design/catalog-integration.md).
- **AI activity bar + job queue** — one client-side queue runs a single heavy AI job at
  a time (WebGPU can't run two GPU inference loops at once); the rest queue instead of
  being blocked. A bottom bar shows the running job's progress, the queue behind it, and
  a persistent list of completed jobs (cleared manually); queued jobs are cancellable, a
  running one is not. Every AI action — tag-suggest, healthcheck audit, duplicate scan,
  per-field draft/improve, and the Improve-all sweep — routes through it. A
  `beforeunload` guard warns before a refresh would drop in-flight work.
- **Tags suggested as the final step of "Improve all fields"** — after drafting and
  harmonising a work item's fields, the sweep also suggests tags (inline clickable
  chips in the editor, and in the Suggested column), in both the single-item editor and
  the bulk multi-item flow.
- **Orphaned-children healthcheck** — a New/Active/Blocked child under a Closed/Resolved
  parent is flagged (`orphaned_child`): either the parent closed prematurely or the
  children are stranded.
- **Board (Kanban) view for Work Items** — a *View* dropdown (leading the toolbar)
  switches the Work Items screen between the table and a Kanban board, grouping the
  same items into columns **by State** or by any tag axis present in the data
  (`area:` / `product:` / `source:` / …). Cards link out to the provider item and
  carry the edit pencil; the Rule Breaks / Hide empty / flag filters apply to the
  board too. The chosen view persists across refreshes (per view, with the toggles).
- **Required-aware AI tagging** — the tagger is told which tag categories a team
  requires (e.g. `area:*`, `source:*`) and makes a best-effort pick for any the
  item doesn't yet satisfy, instead of staying silent; everything else keeps the
  precision-first "omit when unsure" behaviour.
- **Wildcard candidate expansion** — an open-vocabulary required slot (`area:*`)
  expands into concrete candidate values drawn from the team's keyword/alias
  taxonomy and the values already used across its backlog, so the AI can actually
  fill it rather than being handed a bare pattern it can't match.
- **Keyword hints in the AI prompt** — each candidate carries its configured
  keywords (`area:foo (e.g. bar, baz)`) so the model picks on meaning, not on a
  surface word.
- **Capability-tiered AI auto-configuration** — on first load the platform
  (WebGPU / CPU / CUDA, RAM, cores) is detected and each local model is sized to
  it out of the box, via a new `POST /api/llm-config/autotune`; a hand-edited LLM
  registry is never overwritten.
- **WebGPU model-load fallback ladder** — if the chosen model won't fit in VRAM,
  it steps down (7B → 3B → 1.5B → 0.5B) and retries, so an optimistic pick is safe.
- **Underspecified detection** — an item with too little body to tag from gets a
  configurable refine tag suggested and is skipped by the AI (so it can't invent
  an area from nothing), tuned by `refine_tag` / `refine_min_chars`.
- **Per-column checkbox filters** — a funnel dropdown with *Select all* and a
  search box, on the work-item (State, Type, Team, Assignee, Tags), pull-request
  (Repo, Status, Author, Target) and pipeline (Status) tables — no need to learn
  the `!exclude` syntax.
- **Reload nudge on redeploy** — a long-open tab notices a new build (via the
  `/api/health` version stamp) and offers a one-click reload.
- **In-app work-item field editor** — a modal (opened from a pencil in the Work
  Items title) to read and edit a work item's provider fields. On Azure DevOps the
  editable set is discovered live from the item's type — a Bug's *Repro Steps*, a
  Story's *Acceptance Criteria*, custom fields, priority/severity pick-lists — with
  HTML round-tripped to markdown; GitHub and GitLab expose title + body. Rich text
  is edited as markdown with a formatting toolbar and an Edit/Preview toggle. Only
  changed fields are written back, explicit and user-initiated.
- **AI field drafting** — a per-field *Draft / Improve* that writes or refines a
  field from the item's context (type, title, sibling fields, team background). It
  runs on the same AI backend as tagging: a server-side online model, or the
  browser's WebGPU model when that's what's configured.
- **Relation-driven tag suggestions** — a child inherits its parent's `product:` /
  `area:` tags, a linked repository maps to a tag (`repo_tags`), and an item moved
  in from another board is tagged with a configurable source — all deterministic,
  computed on read.
- **GitHub / GitLab field write-back** — issues and merge requests can now be
  edited (title + body); previously these providers were read-only.
- **Configurable AI suggestion cap** — `max_suggestions` bounds how many tags the
  AI proposes per item, defaulting to a value that scales with the number of
  required tag categories so a multi-axis taxonomy isn't truncated (replaces a
  fixed cap of three).
- **Placeholder-aware refine detection** — the "too thin to tag" check strips
  pasted hyperlinks before measuring and matches configurable placeholder phrases
  (e.g. "to be clarified"), so a stub padded out by a long URL is still flagged.
- **Persistent table filters** — each table remembers its per-column filters and
  sort across reloads (local storage).
- **Empty-body flag** — open work items with an empty/very thin description are
  flagged (the most upstream hygiene gap), with a dashboard count and a *Hide empty
  body* toggle so you can work the items that have content first.
- **Healthcheck flags (deterministic)** — opt-in detection of **duplicate titles**
  (likely raised-twice items, excluding resolved/recurring work) and **placeholder
  titles** (`test`, `asdf`, `Untitled`, …, via `bad_title_terms`). Surface as flags,
  so they reuse the chips, dashboard counts, and filters.
- **Near-duplicate scan** — a *Find duplicates* action that scans the whole backlog
  for **reworded** duplicate titles (e.g. "Configure Istio alerting" vs "Set up
  alerting for Istio") that the exact-title check misses. Deterministic TF-IDF cosine
  over titles (IDF down-weights backlog-common words, blocking keeps it scalable), no
  model needed; surfaces as `near_duplicate` flags with the matches + similarity score.
  Threshold per team via `near_duplicate_threshold` (default 0.7).
- **AI healthcheck (on-demand)** — a *Run healthcheck* action over the selected
  work items that asks the model to judge each item's **data quality** (a vague
  title, a description that contradicts the title, boilerplate left unfilled) and
  stores the concerns as advisory `ai_audit` flags. Runs on the active AI backend —
  a server-side online model as a background job, or the browser's WebGPU model via
  the same value-or-prompt handshake as field drafting (the server hands out the
  prompts, the browser runs them, replies are re-parsed + stored server-side).
- **Markdown field editor** — a formatting toolbar + Edit/Preview toggle on rich
  fields; AI drafts land in a **review pane** (Use / Discard) rather than overwriting,
  operate on the live unsaved editor state, and have malformed links auto-repaired.
- **"Improve all fields"** — a top-level editor action that drafts/improves every
  AI-eligible field (the same per-field calls), then runs one **consistency sweep**
  over the whole item so the fields share terminology and don't contradict or repeat
  each other. Every result lands in its field's review pane to keep or discard
  individually — nothing is auto-applied. Runs on the active AI backend (server or
  the browser's WebGPU model via the value-or-prompt handshake).
- **Given-When-Then Acceptance Criteria** — the AI now writes/refines an *Acceptance
  Criteria* field as Given-When-Then scenarios by default (the house style), instead
  of a free-form checklist. Per-team via `acceptance_criteria_style` (`"checklist"` /
  `"plain"` opts out); only the AC field's prompt changes.
- **Sticky toolbar on Work Items** — the page header and toolbar (Rule Breaks, Hide
  empty body, Suggest tags, Run healthcheck, filters) and the table's column headers
  now stay fixed while only the rows scroll, so the actions are always reachable in a
  long backlog instead of requiring a scroll back to the top.

### Fixed
- **Catalog upload now drives tags** — uploading a catalog populated the table but was
  overridden into silence by the hand-maintained `repo_tags` product backfill; that
  backfill is retired and the catalog is now the authoritative product source (broader
  coverage, applied tags intact).
- **AI drafts stop losing information** — a deterministic backstop re-appends any image /
  attachment / link the model dropped when rewriting a field, and strips a field label the
  model echoed into its own value; the field-draft and consistency prompts also forbid it.
- **AI activity bar hides when idle** — an author-`display:flex` was overriding the
  `[hidden]` attribute, so the bar (and a stale full progress bar) lingered with an empty
  queue.
- **Clippy** — two needless-borrow warnings in the GitHub provider's write-back calls.
- **Editor no longer shows fields a work-item type hides** — the field editor
  listed every field *associated* with the type, including ones the process keeps
  off the form (e.g. a Bug's empty *Description*, which Repro Steps replaces). It now
  reads the type's form layout and drops a field only when it's both off-form and
  empty — so hidden empties disappear while any field that holds data is always kept.
- **Azure DevOps bug bodies** — bugs carry their body in *Repro Steps*, not
  *Description* (which is empty for them); the provider now falls back to it, so a
  detailed bug is no longer mis-detected as empty-bodied.
- **Stale AI suggestions** — an item that has become underspecified no longer
  surfaces its old model-guessed tags, and they are pruned from storage on the
  next run.
- Bulk apply-suggestions failures now log full detail to the browser console.
- **"Apply suggestions" applies removals too** — the bulk action previously
  applied only tag adds and rewrites, silently leaving flagged-for-removal tags
  (e.g. a leftover "needs work" tag on a resolved item) in place.
- **No "needs-work" tags suggested on done items** — a resolved / closed / ignored
  item is never offered a refine or other stale-when-resolved tag from any source
  (keyword, alias rewrite, or the refine nudge), so the tool no longer contradicts
  itself by suggesting a tag it would immediately flag.
- **Whole-word keyword matching** — tag keywords match on token boundaries, so a
  short keyword no longer fires inside an unrelated word.
- Toast notifications render above modal dialogs instead of behind them.

### Changed
- **POSEIDEN → POSEIDON** — corrected the misspelling across the entire codebase (crates,
  binaries, Helm chart, deploy scripts, docs) and renamed the GitHub repository.
- **AI field-draft prompts** — a Title is drafted as one short, distinctive line (no
  trailing "update X if necessary" clause), and the consistency pass is now a *minimal
  alignment* (agree terminology, resolve contradictions) rather than a licence to rewrite,
  condense, or drop content.
- **All AI invocations are queued** — every GPU inference (including the editor's
  per-field draft/improve and the Improve-all consistency pass) routes through the single
  activity queue, replacing the old boolean busy-lock that just disabled the other buttons.
- **Work Items toolbar restructured** — grouped with hairline separators (view · filters ·
  search · actions · count), the selection count/clear moved solely to the selection bar,
  the ambiguous "Clear" relabelled "Clear filters", and consistent control sizing.
- The seeded LLM registry sizes its on-device models to the detected platform
  instead of a fixed small default.
- **Centralized client-side AI dispatch** — the browser's "which model, run where"
  decision lives in one module shared by tagging and field drafting, mirroring the
  server's single AI trait.

## [0.1.0] - 2026-08-11

Initial public release. POSEIDON is a Product Owner support tool that keeps a
work-tracker backlog legible: it polls your tracker and CI on a schedule,
validates work items and pipelines against config-defined hygiene rules,
surfaces what needs attention, and reports flow over time. One Rust core, one
logic layer, three shells (hosted web, desktop, CLI).

### Providers
- Poll **Azure DevOps, GitHub, and GitLab** through a provider-agnostic core.
  Public GitHub / GitLab repos poll with no token; a token is optional (private
  repos or higher rate limits).
- Each provider normalises into the same `WorkItem` / `Pipeline` / `PullRequest`
  shapes (Azure DevOps work items + build pipelines + PRs; GitHub Issues +
  Actions + PRs; GitLab Issues + Pipelines + MRs), covered by captured-fixture
  and live smoke tests.
- A deterministic offline `stub` provider powers demo mode and the end-to-end
  tests.

### Hygiene rules
- Config-driven rules interpreted per team: required tags, allowed / disallowed
  tag lists (with `*` wildcard patterns), untagged detection, per-state
  staleness, a stale-state-tag check, and pull-request + pipeline checks.
- Per-team rule overrides that inherit the instance default.

### Tagging
- Advisory tag suggestions from a deterministic keyword + alias engine (no model
  needed) and an optional AI tagger; add / rewrite / remove chips, applied only
  by a person (POSEIDON never auto-applies).
- AI backends: on-device embedded model (candle / Qwen2.5, CPU or CUDA),
  in-browser WebGPU, or a hosted provider (Claude / OpenAI / Gemini). Configured
  in a reorderable priority list.

### Views, reports, and Recap
- Dashboard, Work Items (sortable / filterable / inline-editable table), Pull
  Requests, Pipelines, Rules, Reports, and Recap.
- A configurable **report engine** (datasource + conditions + render type) that
  also drives the home velocity tiles; export to PDF / PNG / CSV.
- **Recap**: a shareable highlights deck generated from your closed work, grouped
  by `area:` / `source:` and internal vs external, exportable as a single
  self-contained HTML file.

### Write-back (Azure DevOps)
- User-initiated inline State / Tags edits, a bounded multi-select bulk apply,
  and work-item to pull-request links, each written through the provider. Write-
  back is Azure DevOps only today; GitHub / GitLab are read-focused.

### Deployment and distribution
- Ships as a slim Docker image, a Helm chart (nginx or Istio, oauth2-proxy,
  bundled Dex / Keycloak for a self-contained PoC), a single desktop / CLI
  binary, and a repointable web / mobile client.
- Portable mode confines every write beneath `./.portable/`.
- GitHub Pages landing page with a client-side demo mode (sample data, no
  backend).

### CLI
- `poseidon poll` / `lint` / `report` / `config` / `tag`. `lint` exits non-zero
  on error-severity flags for CI gating; `tag` generates advisory suggestions
  (build `--features cuda` for GPU tagging).

### Platform
- Multi-tenant owner scoping (identity from the `X-Auth-Request-Email` header,
  `default` owner when unauthenticated); per-owner teams, rules, tags, reports.
- Native device-code OAuth sign-in for Azure DevOps (no `az` CLI dependency).
- A self-checking Doctor (traffic-light health, per-provider access checks,
  update check) and centralised telemetry (console, rolling file, OTLP).

[0.1.0]: https://github.com/andrewiankidd/POSEIDON/releases/tag/v0.1.0
