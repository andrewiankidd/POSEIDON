# Changelog

POSEIDON ships as a **single rolling build** — no tagged versions, no semantic
versioning. Every push to `main` rebuilds one `latest-main` artifact, so the
**short commit SHA is the version number**.

Each entry below is headed by the SHA (+ date) that shipped it, newest first,
mirroring this file's own commit history. Within an entry, changes are grouped
**Added / Changed / Fixed**.

> **Note (2026-08-31):** history was rewritten to scrub stray strings from old
> doc revisions, so every SHA below this line changed. Entries have been
> remapped. If you have an old clone, re-clone or
> `git fetch && git reset --hard origin/main`.

## [e6290e5] — 2026-08-25

### Added
- **Claude Code desktop-app AI backend** — `ClaudeCodeTagger` shells out to the
  `claude` CLI bundled with the Claude Code desktop app, reusing its existing
  OAuth session, so this backend needs **no API key**. First unauthenticated use
  auto-triggers the `claude auth login` browser flow, and when a call returns
  zero tags the raw model response is surfaced in the AI activity log. Windows
  wrinkles handled: the versioned bundled `claude.exe` is invoked directly
  (the Squirrel launcher routes via Electron IPC and produces no subprocess
  stdout), and `CREATE_NO_WINDOW` suppresses console flicker on every call.

### Changed
- **Desktop app restores the last-visited page on launch** instead of always
  opening on the dashboard.

## [53877e0] — 2026-08-24

### Added
- **Type + Assignee filters on Work Items** — two facet dropdowns in the shared
  toolbar that filter **both** the table and the board (they drive the one predicate
  those views share), so the board finally has the Type/Assignee filtering that used
  to exist only as table column filters. Built from the values present in the data,
  persisted across reloads, and cleared by "Clear filters".

### Fixed
- **Qwen3 `<think>` block leaking into AI output** — Qwen3 emits an empty
  `<think></think>` even with `/no_think`, and it was showing up in Bulk-improve
  drafts, tag JSON, and the healthcheck audit. Stripped wherever raw model text is
  consumed (chat, tagging, audit parse).

## [030e726] — 2026-08-19

### Fixed
- **Suggested-tag chips rendered `[object Object]`** — the WebGPU tagger returns
  `{ tag, reason }` objects; the AI activity-queue per-item list and the editor's
  inline "+tag" chips stringified the object instead of showing the tag name (and the
  editor chip would have written the object back to the provider on click). Both now
  normalise to the tag name; server storage still receives the full objects.

## [14a57b1] — 2026-08-19

### Added
- **Durable AI activity + field drafts** — the "Improve all fields" queue and each
  item's per-field drafts are persisted server-side (`ai_activity` + `ai_field_drafts`,
  migration `0007`) and rehydrated on load, so a refresh (or another machine) keeps the
  queue, the ✨ badges, and an audit trail. Improve-all also gains a **"Force re-do all"**
  split button — the plain click stays resumable (skips items that already have drafts),
  the caret regenerates everything.
- **Desktop command parity** — the work-item field editor and the AI activity/draft
  endpoints are exposed as Tauri commands (previously HTTP-only), so the standalone
  desktop app matches the hosted web build.

### Changed
- **Offline models switched Qwen2.5 → Qwen3** (0.6B / 1.7B / 4B / 8B; 4B is the auto
  ceiling, 8B hand-pick). `OFFLINE_MODELS` is now the single source of truth: the WebGPU
  model-id map, the OOM fallback ladder, `recommend_model`, the seeded catalog and the
  starter templates all derive from it, and a drift-guard test pins the demo fixture — so
  swapping the model family is a one-place edit.

### Fixed
- **Form-control desync (`el()` boolean attributes)** — booleans like `selected` /
  `disabled` / `checked` were written with `setAttribute`, which marks the attribute
  *present* even for `false`, so every `<select>` option ended up selected and the
  control showed its last option while the saved value stayed unchanged. This was the
  root cause of the model picker "shows X but saves Y". Booleans are now set as
  properties.
- **AI config self-heal** — a stored `offline_model` id that's no longer in the catalog
  (e.g. after the Qwen3 switch) is coerced to the current recommended model on load and
  re-persisted, so a model swap can't strand a saved integration on a dead id.
- **Blocked children no longer flagged as orphaned** (`orphaned_child_ignore_states`).

## [3b6dfe7] — 2026-08-18

### Added
- **Portable single-binary distribution** — the desktop build ships a
  self-contained executable (the frontend is embedded) named
  `poseidon-portable-<os>` alongside the installers. The word *portable* in the
  filename is itself the trigger: `poseidon-paths` detects it and confines every
  write beneath `./.portable/` with no flag or sentinel needed. A portable build
  also badges its window title **POSEIDON [Portable]**. Single executables attach
  to the release raw (never zipped), so the download links the binary directly.

### Changed
- **Docs + screenshots regenerated against the demo tenant** — the feature docs
  and all in-app screenshots are captured from the seeded demo tenant
  (`poseidon+demo-data@example.com`) through Dex, so a real backlog is never in
  frame; docs now cover the board view, field editor, service catalog, the
  healthchecks, and the AI activity queue.
- **Changelog restructured** — dropped the fake SemVer / Keep-a-Changelog framing
  and the phantom `0.1.0` tag; entries are now marked by commit SHA + date, since
  the project ships one rolling `latest-main` build with no version tags.

### Fixed
- **Org names scrubbed from tracked source** — real team/product names that had
  leaked into committed docs and config examples are replaced with synthetic
  placeholders; the real values stay in the git-ignored tenant import files.
- **Migration line-endings pinned to LF** (`.gitattributes`) — a CRLF flip on a
  migration file changes its bytes and trips sqlx's embedded-migration checksum
  (a fatal "previously applied but has been modified" on boot).

## [c287d41] — 2026-08-17

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
  running one is not. A `beforeunload` guard warns before a refresh would drop in-flight
  work.
- **Board (Kanban) view for Work Items** — a *View* dropdown (leading the toolbar)
  switches the Work Items screen between the table and a Kanban board, grouping the
  same items into columns **by State** or by any tag axis present in the data
  (`area:` / `product:` / `source:` / …). Cards link out to the provider item and
  carry the edit pencil; the Rule Breaks / Hide empty / flag filters apply to the
  board too. The chosen view persists across refreshes.
- **In-app work-item field editor** — a modal (opened from a pencil in the Work
  Items title, or a board card) to read and edit a work item's provider fields. On
  Azure DevOps the editable set is discovered live from the item's type — a Bug's
  *Repro Steps*, a Story's *Acceptance Criteria*, custom fields, priority/severity
  pick-lists — with HTML round-tripped to markdown; GitHub and GitLab expose title +
  body. Only changed fields are written back, explicit and user-initiated.
- **AI field drafting** — a per-field *Draft / Improve* that writes or refines a
  field from the item's context (type, title, sibling fields, team background), on the
  same AI backend as tagging (a server-side online model, or the browser's WebGPU model).
- **"Improve all fields"** — a top-level editor action that drafts/improves every
  AI-eligible field, then runs one **consistency sweep** over the whole item so the
  fields share terminology and don't contradict or repeat each other, and finally
  suggests tags. Every result lands in its field's review pane to keep or discard
  individually — nothing is auto-applied. Works in both the single-item editor and the
  bulk multi-item flow.
- **Markdown field editor** — a formatting toolbar + Edit/Preview toggle on rich
  fields; AI drafts land in a **review pane** (Use / Discard) rather than overwriting,
  operate on the live unsaved editor state, and have malformed links auto-repaired.
- **Given-When-Then Acceptance Criteria** — the AI writes/refines an *Acceptance
  Criteria* field as Given-When-Then scenarios by default (the house style), instead
  of a free-form checklist. Per-team via `acceptance_criteria_style`.
- **Relation-driven tag suggestions** — a child inherits its parent's `product:` /
  `area:` tags, a linked repository maps to a tag (`repo_tags`), and an item moved
  in from another board is tagged with a configurable source — all deterministic,
  computed on read.
- **GitHub / GitLab field write-back** — issues and merge requests can now be
  edited (title + body); previously these providers were read-only.
- **AI healthcheck (on-demand)** — a *Run healthcheck* action over the selected
  work items that asks the model to judge each item's **data quality** (a vague
  title, a description that contradicts the title, boilerplate left unfilled) and
  stores the concerns as advisory `ai_audit` flags.
- **Near-duplicate scan** — a *Find duplicates* action that scans the whole backlog
  for **reworded** duplicate titles that the exact-title check misses. Deterministic
  TF-IDF cosine over titles, no model needed; surfaces as `near_duplicate` flags with
  the matches + similarity score. Threshold per team via `near_duplicate_threshold`.
- **Deterministic healthcheck flags** — opt-in detection of **duplicate titles** and
  **placeholder titles** (`test`, `asdf`, `Untitled`, …, via `bad_title_terms`),
  surfaced as flags that reuse the chips, dashboard counts, and filters.
- **Orphaned-children healthcheck** — a New/Active/Blocked child under a Closed/Resolved
  parent is flagged (`orphaned_child`): either the parent closed prematurely or the
  children are stranded.
- **Sticky toolbar on Work Items** — the page header, toolbar, and table column headers
  stay fixed while only the rows scroll, so the actions stay reachable in a long backlog.

### Changed
- **POSEIDEN → POSEIDON** — corrected the misspelling across the entire codebase (crates,
  binaries, Helm chart, deploy scripts, docs) and renamed the GitHub repository.
- **All AI invocations are queued** — every GPU inference (including the editor's
  per-field draft/improve and the Improve-all consistency pass) routes through the single
  activity queue, replacing the old boolean busy-lock that just disabled the other buttons.
- **Work Items toolbar restructured** — grouped with hairline separators (view · filters ·
  search · actions · count), the selection count/clear moved solely to the selection bar,
  the ambiguous "Clear" relabelled "Clear filters", and consistent control sizing.
- **AI field-draft prompts** — a Title is drafted as one short, distinctive line, and the
  consistency pass is now a *minimal alignment* (agree terminology, resolve contradictions)
  rather than a licence to rewrite, condense, or drop content.

### Fixed
- **Catalog upload now drives tags** — uploading a catalog populated the table but was
  overridden into silence by the hand-maintained `repo_tags` product backfill; that
  backfill is retired and the catalog is now the authoritative product source.
- **AI drafts stop losing information** — a deterministic backstop re-appends any image /
  attachment / link the model dropped when rewriting a field, and strips a field label the
  model echoed into its own value; the prompts also forbid it.
- **AI activity bar hides when idle** — an author-`display:flex` was overriding the
  `[hidden]` attribute, so the bar (and a stale full progress bar) lingered with an empty
  queue.
- **Editor no longer shows fields a work-item type hides** — it now reads the type's form
  layout and drops a field only when it's both off-form and empty (e.g. a Bug's empty
  *Description*, which Repro Steps replaces).
- **Azure DevOps bug bodies** — bugs carry their body in *Repro Steps*, not *Description*;
  the provider now falls back to it, so a detailed bug isn't mis-detected as empty-bodied.
- **"Apply suggestions" applies removals too** — the bulk action previously applied only
  tag adds and rewrites, silently leaving flagged-for-removal tags in place.
- Bulk apply-suggestions failures now log full detail to the browser console.
- **Clippy** — needless-borrow warnings in the GitHub provider's write-back calls.
- Toast notifications render above modal dialogs instead of behind them.

## [510f677] — 2026-08-12

### Added
- **Capability-tiered AI auto-configuration** — on first load the platform
  (WebGPU / CPU / CUDA, RAM, cores) is detected and each local model is sized to it out
  of the box, via `POST /api/llm-config/autotune`; a hand-edited LLM registry is never
  overwritten. If the chosen WebGPU model won't fit in VRAM it steps down the ladder
  (7B → 3B → 1.5B → 0.5B) and retries.
- **Required-aware AI tagging** — the tagger is told which tag categories a team
  requires (e.g. `area:*`, `source:*`) and makes a best-effort pick for any the item
  doesn't yet satisfy. An open-vocabulary required slot (`area:*`) expands into concrete
  candidate values drawn from the team's taxonomy and existing backlog values, and each
  candidate carries its configured keywords (`area:foo (e.g. bar, baz)`) so the model
  picks on meaning. Everything else keeps the precision-first "omit when unsure" behaviour.
- **Underspecified detection** — an item with too little body to tag from gets a
  configurable refine tag suggested and is skipped by the AI, tuned by `refine_tag` /
  `refine_min_chars`; the check strips pasted hyperlinks before measuring and matches
  configurable placeholder phrases (e.g. "to be clarified").
- **Configurable AI suggestion cap** — `max_suggestions` bounds how many tags the AI
  proposes per item, defaulting to a value that scales with the number of required tag
  categories so a multi-axis taxonomy isn't truncated.
- **Per-column checkbox filters** — a funnel dropdown with *Select all* and a search box,
  on the work-item, pull-request and pipeline tables — no need to learn the `!exclude`
  syntax. Each table also remembers its per-column filters and sort across reloads.
- **Empty-body flag** — open work items with an empty/very thin description are flagged
  (the most upstream hygiene gap), with a dashboard count and a *Hide empty body* toggle.
- **Reload nudge on redeploy** — a long-open tab notices a new build (via the
  `/api/health` version stamp) and offers a one-click reload.

### Changed
- The seeded LLM registry sizes its on-device models to the detected platform instead of
  a fixed small default.
- **Centralized client-side AI dispatch** — the browser's "which model, run where"
  decision lives in one module shared by tagging and field drafting, mirroring the
  server's single AI trait.

### Fixed
- **Stale AI suggestions** — an item that has become underspecified no longer surfaces its
  old model-guessed tags, and they are pruned from storage on the next run.
- **No "needs-work" tags suggested on done items** — a resolved / closed / ignored item is
  never offered a refine or other stale-when-resolved tag from any source, so the tool no
  longer contradicts itself by suggesting a tag it would immediately flag.
- **Whole-word keyword matching** — tag keywords match on token boundaries, so a short
  keyword no longer fires inside an unrelated word.

## [50c9f12] — 2026-08-11

Initial public release. POSEIDON is a Product Owner support tool that keeps a
work-tracker backlog legible: it polls your tracker and CI on a schedule,
validates work items and pipelines against config-defined hygiene rules,
surfaces what needs attention, and reports flow over time. One Rust core, one
logic layer, three shells (hosted web, desktop, CLI).

### Added
- **Providers** — poll **Azure DevOps, GitHub, and GitLab** through a
  provider-agnostic core; each normalises into the same `WorkItem` / `Pipeline` /
  `PullRequest` shapes. Public GitHub / GitLab repos poll with no token. A
  deterministic offline `stub` provider powers demo mode and the e2e tests.
- **Hygiene rules** — config-driven per team: required tags, allowed / disallowed
  tag lists (with `*` wildcards), untagged detection, per-state staleness, a
  stale-state-tag check, and pull-request + pipeline checks; per-team overrides
  inherit the instance default.
- **Tagging** — advisory suggestions from a deterministic keyword + alias engine
  (no model needed) and an optional AI tagger; add / rewrite / remove chips,
  applied only by a person. AI backends: on-device embedded (candle / Qwen2.5),
  in-browser WebGPU, or a hosted provider (Claude / OpenAI / Gemini).
- **Views, reports, and Recap** — Dashboard, Work Items, Pull Requests, Pipelines,
  Rules, Reports, and a Recap highlights deck. A configurable report engine
  (datasource + conditions + render type) drives the home velocity tiles; export
  to PDF / PNG / CSV.
- **Write-back (Azure DevOps)** — user-initiated inline State / Tags edits, a
  bounded multi-select bulk apply, and work-item to pull-request links, each
  written through the provider (Azure DevOps only; GitHub / GitLab are read-focused).
- **Deployment** — a slim Docker image, a Helm chart (nginx or Istio, oauth2-proxy,
  bundled Dex / Keycloak), a single desktop / CLI binary, and a repointable
  web / mobile client. Portable mode confines every write beneath `./.portable/`.
  GitHub Pages landing page with a client-side demo mode.
- **CLI** — `poseidon poll` / `lint` / `report` / `config` / `tag`; `lint` exits
  non-zero on error-severity flags for CI gating.
- **Platform** — multi-tenant owner scoping (identity from `X-Auth-Request-Email`,
  `default` when unauthenticated); native device-code OAuth sign-in for Azure
  DevOps; a self-checking Doctor and centralised telemetry (console, file, OTLP).

[3b6dfe7]: https://github.com/andrewiankidd/POSEIDON/commit/3b6dfe7
[c287d41]: https://github.com/andrewiankidd/POSEIDON/commit/c287d41
[510f677]: https://github.com/andrewiankidd/POSEIDON/commit/510f677
[50c9f12]: https://github.com/andrewiankidd/POSEIDON/commit/50c9f12
