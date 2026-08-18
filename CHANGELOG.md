# Changelog

POSEIDON ships as a **single rolling build** — no tagged versions, no semantic
versioning. Every push to `main` rebuilds one `latest-main` artifact.

This file is only the **human-readable** layer: what changed, in plain language.
It deliberately carries no version headers, dates, or commit SHAs — the file's
own git history is the source of truth for *when* each entry landed and *which*
commit shipped it (`git log --oneline CHANGELOG.md`, or `git blame` a line).
New entries go at the top of their group.

## Added
- **Portable single-binary distribution** — the desktop build ships a
  self-contained executable (the frontend is embedded) named
  `poseidon-portable-<os>` alongside the installers. The word *portable* in the
  filename is itself the trigger: `poseidon-paths` detects it and confines every
  write beneath `./.portable/` with no flag or sentinel needed. A portable build
  also badges its window title **POSEIDON [Portable]**. Single executables attach
  to the release raw (never zipped), so the download links the binary directly.
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
  running one is not. Every AI action routes through it. A `beforeunload` guard warns
  before a refresh would drop in-flight work.
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
  body. Rich text is edited as markdown with a formatting toolbar and an Edit/Preview
  toggle. Only changed fields are written back, explicit and user-initiated.
- **AI field drafting** — a per-field *Draft / Improve* that writes or refines a
  field from the item's context (type, title, sibling fields, team background). It
  runs on the same AI backend as tagging: a server-side online model, or the
  browser's WebGPU model when that's what's configured.
- **"Improve all fields"** — a top-level editor action that drafts/improves every
  AI-eligible field, then runs one **consistency sweep** over the whole item so the
  fields share terminology and don't contradict or repeat each other, and finally
  suggests tags. Every result lands in its field's review pane to keep or discard
  individually — nothing is auto-applied. Works in both the single-item editor and
  the bulk multi-item flow.
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
- **Orphaned-children healthcheck** — a New/Active/Blocked child under a Closed/Resolved
  parent is flagged (`orphaned_child`): either the parent closed prematurely or the
  children are stranded.
- **Healthcheck flags (deterministic)** — opt-in detection of **duplicate titles** and
  **placeholder titles** (`test`, `asdf`, `Untitled`, …, via `bad_title_terms`), surfaced
  as flags that reuse the chips, dashboard counts, and filters.
- **Empty-body flag** — open work items with an empty/very thin description are flagged
  (the most upstream hygiene gap), with a dashboard count and a *Hide empty body* toggle.
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
- **Reload nudge on redeploy** — a long-open tab notices a new build (via the
  `/api/health` version stamp) and offers a one-click reload.
- **Sticky toolbar on Work Items** — the page header, toolbar, and table column headers
  stay fixed while only the rows scroll, so the actions stay reachable in a long backlog.

## Changed
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
- **Centralized client-side AI dispatch** — the browser's "which model, run where"
  decision lives in one module shared by tagging and field drafting, mirroring the
  server's single AI trait. The seeded LLM registry sizes its on-device models to the
  detected platform instead of a fixed small default.
- **Docs + screenshots captured from the demo tenant** — the feature docs and all in-app
  screenshots come from the seeded demo tenant through Dex, so a real backlog is never in
  frame; org names in tracked source are synthetic placeholders (real values stay in the
  git-ignored tenant import files).

## Fixed
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
- **Stale AI suggestions** — an item that has become underspecified no longer surfaces its
  old model-guessed tags, and they are pruned from storage on the next run.
- **No "needs-work" tags suggested on done items** — a resolved / closed / ignored item is
  never offered a refine or other stale-when-resolved tag from any source, so the tool no
  longer contradicts itself by suggesting a tag it would immediately flag.
- **Whole-word keyword matching** — tag keywords match on token boundaries, so a short
  keyword no longer fires inside an unrelated word.
- **Migration line-endings pinned to LF** (`.gitattributes`) — a CRLF flip changes a
  migration file's bytes and trips sqlx's embedded-migration checksum on boot.
- Bulk apply-suggestions failures now log full detail to the browser console.
- **Clippy** — needless-borrow warnings in the GitHub provider's write-back calls.
- Toast notifications render above modal dialogs instead of behind them.

## Baseline

The initial scaffold: a Product Owner support tool that keeps a work-tracker
backlog legible — polls Azure DevOps / GitHub / GitLab through a
provider-agnostic core, validates work items + pipelines against config-defined
hygiene rules, surfaces what needs attention, and reports flow over time. One
Rust core, one logic layer, three shells (hosted web, desktop, CLI): the
provider normalisation, the config-driven rules engine, deterministic + AI tag
suggestions, the report engine + Recap deck, Azure DevOps write-back, multi-tenant
owner scoping, device-code sign-in, the Doctor, centralised telemetry, the Docker
image + Helm chart, and the GitHub Pages demo. See the git history for the detail.
