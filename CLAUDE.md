# CLAUDE.md

Working notes on **how** POSEIDON is built and the principles to honour when
changing it. The README + `docs/` cover *what* POSEIDON is and where it's going;
this file captures the constraints those decisions fit inside.

## Mission, in one breath

A Product Owner support tool that keeps a backlog legible. Polls Azure DevOps,
GitHub, and GitLab through a provider-agnostic core, validates work items +
pipelines against config-defined rules, surfaces what needs attention, and
reports flow over time. Runs as a
hosted web instance or a standalone/repointed desktop-mobile client - one codebase, one
logic layer, several shells.

## Build principles

### 1. Provider-agnostic core

Azure DevOps was the first provider, but nothing in `poseidon-core` names it. A
`WorkItem` is just id + title + state + tags + timestamps. Azure DevOps, GitHub,
and GitLab all ship, each normalising *into* the core shapes via its own
`impl Provider` (GitHub Issues/Actions/PRs, GitLab Issues/Pipelines/MRs); further
providers (Jira, Linear) add another `impl` and nothing else in the system
changes. If you find yourself reaching for a provider-specific concept outside
`poseidon-providers`, it's in the wrong layer.

### 2. Rules live in config, not code

Every hygiene decision - required tags, allowed/denied tags, staleness limits,
ignored states/types - is **data**, interpreted by `poseidon-rules`, stored
per-owner in the DB (edited in the app, or via `poseidon config import`). Adding
a policy knob means extending the `RuleSet` schema + the engine, never
hard-coding a specific team's convention. A team tunes its policy live - no code
change, no restart.

### 3. One `Service`, three transports

`poseidon-server`'s `Service` is the single logic layer. The axum handlers, the
Tauri invoke handlers, and the CLI all call it - logic exists exactly once, so
the web instance, the desktop app, and the CLI can never drift. When you add an
operation, add it to `Service` and expose it through each transport as a thin
wrapper. Never put logic in a handler.

### 4. Visibility first

POSEIDON is visibility-first: it polls the tracker, evaluates hygiene, and shows
what needs attention. It also supports a small, explicit set of user-initiated
write-backs to the provider (the Azure DevOps provider today; GitHub / GitLab are
read-focused) - inline work-item State/Tags edits
(`Service::update_work_item`), a hand-picked multi-select bulk apply of those same
fields (the frontend loops `update_work_item` over the rows the user ticked - no
new Service verb, no server-side sweep), and work-item↔PR links
(`Service::link_work_item_pr`) - each triggered by a person, never a silent side
effect of polling. It deliberately does *not* trigger builds, *auto/policy-driven*
mass mutation (it will not auto-apply its own AI tag suggestions or sweep the
backlog on a rule), close items en masse, reassign owners, or send messages, which
keeps the scope tight and the tool complementary to the workflow tools teams
already use - see [docs/SCOPE.md](docs/SCOPE.md). The bulk bar is bounded by the
same rule: N explicit single-item edits a person chose, not a board robot. New
write-back actions stay explicit, opt-in, and clearly scoped. POSEIDON also freely
writes its *own* local state (its SQLite DB).

### 5. Portable-first - never write outside expected paths

All writable locations resolve through `poseidon-paths`. Portable mode
(`POSEIDON_PORTABLE_MODE=true` or a `.portable` sentinel beside the binary)
confines every write - DB, logs, cache - beneath `./.portable/`. The container
uses `POSEIDON_DATA_DIR` to point at a mounted volume. Nothing is ever written to
an unexpected location. If you add a new writable artifact, route it through
`Paths`, don't `std::fs` a path directly.

### 6. Secrets stay in the environment

A provider credential (the Azure DevOps PAT, or an optional GitHub / GitLab token
- public GitHub / GitLab repos need none) is read from an env var named in config
- never stored in config, never persisted to the DB, never sent to the browser.
It's read only via `Service::resolve_credential`, used by `poll_once` and by every
provider write/lookup path (`provider_for`). For Azure DevOps, polling needs only
Work Items Read + Build Read; the write-back features (inline State/Tags edits,
WI↔PR links - Azure DevOps only) additionally need Work Items Write - don't ask
for more scope than the enabled features require.

### 7. Single binary + Docker image

Desktop/CLI ship as a single binary; the web version as a slim Docker image.
SQLite is statically bundled (no runtime DB package) and migrations are embedded
(`sqlx::migrate!`) so first run auto-provisions the schema wherever the DB lands.
Keep it that way - no external service dependency for the core product.

### 8. Tests pin behaviour, not implementation

`poseidon-rules`, the store's report aggregates, and the provider normalisation
are the high-value test targets - pure functions with concrete I/O contracts,
written test-first. A test should survive a refactor that preserves the
user-visible contract and fail when the contract changes. UX glue and the
frontend are tested lightly on purpose.

## Storage / persistence

- **Local/desktop**: SQLite under the OS app-data dir (`directories` crate).
- **K8s/web**: SQLite on a mounted PV/PVC (`POSEIDON_DATA_DIR=/data`).
  ReadWriteOnce, single replica - SQLite is a single writer. Horizontal scale is
  the Postgres-store swap (`Store`'s typed surface is the seam - a second impl of
  the same shape), not more replicas.
- **Portable**: everything under `./.portable/`.
- **No config file.** Instance settings come from env vars
  (`POSEIDON_BIND_ADDR`/`_PORT`/`_POLL_INTERVAL`, `POSEIDON_OTLP_*`/`_LOG_*`);
  per-owner config (teams/rules/reports) lives in the DB, set via the UI or
  `poseidon config import` (portable YAML).
- **Pre-1.0 migrations = greenfield.** Until 1.0 tagged binaries ship, there's no
  data in the wild: edit `0001_init.sql` in place as the schema evolves and wipe
  + re-provision the DB. No additive migrations, no backward-compat shims, no
  legacy config aliases. Additive migrations begin only once real users have
  real data (post-1.0).

## Multi-tenant (owner scoping)

Every stored row carries an `owner` column. Auth is delegated to the deployment
(oauth2-proxy / ingress / Istio ext-authz), which injects the user's email as the
`X-Auth-Request-Email` header; the axum `Scoped` extractor maps it to the
request's `owner`. So each authenticated user has their **own** teams, rules,
tags, and reports - per-owner config lives in the `user_config` DB table (a JSON
blob), and every read/write is owner-scoped. With no header (standalone desktop /
CLI / unauthenticated local), the owner falls back to `DEFAULT_OWNER` (`"default"`)
- one tenant, unchanged behaviour. Instance-level settings (bind/port/poll,
telemetry) come from **env vars**; there is no config file, and a fresh owner
(including `default`) starts empty - configured via the UI or `config import`.
The ADO credential is resolved per-owner (`Service::resolve_credential`): a team
PAT (env) wins if set, else the Azure CLI. `az` is made multi-user-safe by
per-owner isolation - each owner gets its own `AZURE_CONFIG_DIR` under
`<data>/az-sessions/<owner>/` (the `default` owner uses the machine `~/.azure`),
so hosted users can device-code sign in from the web without sharing a token
cache. A shared PAT / service credential is still supported (and takes
precedence). Don't remove owner scoping "because standalone only uses default" -
it's what makes the hosted instance multi-user.

## Active conventions

- **Conventional commits** - `feat:` / `fix:` / `ci:` / `docs:` / `refactor:` /
  `chore:` / `test:`, with a crate scope where it helps (`feat(rules): …`).
- **Branch protection on `main`** - CI status checks gate merges.
- **Auto-merge for patch dependabot bumps**; manual review for minor/major.
- **Pre-push hook** ([.githooks/pre-push](.githooks/pre-push)) mirrors CI's
  static-analysis gate. Enable on a fresh clone:
  ```bash
  git config core.hooksPath .githooks
  cargo install cargo-audit cargo-machete cargo-deny
  ```

## Where things live

- [README.md](README.md) - public pitch + quick-start.
- [docs/PROJECT_STATUS.md](docs/PROJECT_STATUS.md) - what works today.
- [docs/ROADMAP.md](docs/ROADMAP.md) - committed next.
- [docs/BACKLOG.md](docs/BACKLOG.md) - everything considered, loosely ranked.
- [docs/SCOPE.md](docs/SCOPE.md) - what POSEIDON deliberately is not + non-goals.
- [deploy/helm/poseidon/](deploy/helm/poseidon/) - Kubernetes chart (PVC + Istio notes).
